import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import { Methods } from '../../src/protocol';
import type {
  BuildContext,
  CacheStatus,
  ExperimentalFlag,
  IndexStatus,
  InstallationContext,
  ProjectCatalogue,
  ProjectContext,
} from '../../src/protocol';
import {
  browseDestination,
  buildItems,
  buildSelectionMessage,
  directoryKey,
  Generation,
  installationItems,
  installationSelectionMessage,
  isCandidate,
  mainSourceConfirmed,
  projectMenuItems,
  projectMenuPlaceholder,
  relativeLabel,
  sameBuildField,
  sameInstallationContext,
  sameProjectContext,
  SerialQueue,
  summarizeWarnings,
} from '../../src/pickerLogic';

function fixture<T>(name: string): T {
  const json = readFileSync(resolve('test/fixtures/protocol', name), 'utf8');
  return JSON.parse(json) as T;
}

const ambiguous = fixture<ProjectContext>('projectContext.ambiguous.json');
const selected = fixture<ProjectContext>('projectContext.selected.json');
const buildContext = fixture<BuildContext>('buildContext.json');
const installationContext = fixture<InstallationContext>('installationContext.json');
const browsed = fixture<ProjectContext>('selectProject.browsed.json');

const allFlags: Record<ExperimentalFlag, boolean> = {
  projectSelection: true,
  projectCatalogue: true,
  buildSelection: true,
  installationSelection: true,
};

describe('protocol methods', () => {
  it('exposes the server request and notification method names', () => {
    expect(Methods).toEqual({
      projectContext: 'pascal/projectContext',
      selectProject: 'pascal/selectProject',
      listProjects: 'pascal/listProjects',
      buildContext: 'pascal/buildContext',
      selectBuildConfig: 'pascal/selectBuildConfig',
      selectPlatform: 'pascal/selectPlatform',
      installationContext: 'pascal/installationContext',
      selectInstallation: 'pascal/selectInstallation',
      indexStatus: 'pascal/indexStatus',
      cacheStatus: 'pascal/cacheStatus',
    });
  });
});

describe('protocol fixtures', () => {
  it('records Windows project catalogue and selected-project response shapes', () => {
    const catalogue = fixture<ProjectCatalogue>('listProjects.json');
    expect(catalogue.projects[0]?.projectUri).toBe('file:///c%3A/src/Shop/Shop.dproj');
    expect(browsed.selectedProjectUri).toBe('file:///c%3A/src/Shop/Shop.dproj');
    expect(browsed.lintConfigUri).toBeNull();
  });

  it('records pending and complete index counts', () => {
    const pending = fixture<IndexStatus>('indexStatus.pending.json');
    const complete = fixture<IndexStatus>('indexStatus.complete.json');
    expect(pending.contexts[0]?.pending).toBe(28);
    expect(pending.contexts[0]?.complete).toBe(false);
    expect(complete.contexts[0]?.fresh).toBe(840);
    expect(complete.contexts[0]?.complete).toBe(true);
  });

  it('records nullable cache fields for enabled and bypassed states', () => {
    const enabled = fixture<CacheStatus>('cacheStatus.enabled.json');
    const bypassed = fixture<CacheStatus>('cacheStatus.bypassed.json');
    expect(enabled.cause).toBeNull();
    expect(enabled.store?.pressure).toBe('none');
    expect(bypassed.cause?.kind).toBe('lowDiskSpace');
    expect(bypassed.store).toBeNull();
  });
});

describe('directoryKey', () => {
  it('normalizes Windows drive-letter case and percent-encoded colons', () => {
    expect(directoryKey('file:///c%3A/Src/Shop/Shop.dproj'))
      .toBe(directoryKey('file:///C:/src/shop/Main.pas'));
  });

  it('does not equate different Windows directories', () => {
    expect(directoryKey('file:///c%3A/Src/Shop/Shop.dproj'))
      .not.toBe(directoryKey('file:///C:/Src/Other/x.pas'));
  });

  it('keeps POSIX path comparison case-sensitive', () => {
    expect(directoryKey('file:///home/u/A/x.pas'))
      .not.toBe(directoryKey('file:///home/u/a/x.pas'));
  });
});

describe('relativeLabel', () => {
  it('returns the relative path inside the project scope', () => {
    expect(relativeLabel('file:///home/u/src/App/sub/App.dproj', 'file:///home/u/src/App/'))
      .toBe('sub/App.dproj');
  });

  it('uses the project file name when the scope is unavailable or outside it', () => {
    expect(relativeLabel('file:///home/u/src/App/sub/App.dproj', null)).toBe('App.dproj');
    expect(relativeLabel('file:///home/u/Other/App.dproj', 'file:///home/u/src/App/'))
      .toBe('App.dproj');
  });

  it('matches Windows scope paths despite drive and directory casing', () => {
    expect(relativeLabel('file:///c%3A/Src/Shop/sub/Shop.dproj', 'file:///C:/src/shop/'))
      .toBe('sub/Shop.dproj');
  });
});

describe('project menu', () => {
  it('lists Automatic and project candidates before enabled auxiliary actions', () => {
    expect(projectMenuItems(ambiguous, allFlags)).toEqual([
      { kind: 'project', projectUri: null, label: 'Automatic' },
      { kind: 'project', projectUri: ambiguous.candidates[0], label: 'App.dproj' },
      { kind: 'project', projectUri: ambiguous.candidates[1], label: 'AppTests.dproj' },
      { kind: 'browse', label: 'Browse for project…' },
      { kind: 'installation', label: 'Select Delphi installation…' },
      { kind: 'build-config', label: 'Select build configuration…' },
      { kind: 'platform', label: 'Select platform…' },
    ]);
  });

  it('marks the selected candidate and omits disabled auxiliary actions', () => {
    const context = { ...selected, selectionMode: 'configured' };
    const items = projectMenuItems(context, {
      projectSelection: false,
      projectCatalogue: false,
      buildSelection: false,
      installationSelection: false,
    });

    expect(items).toEqual([
      { kind: 'project', projectUri: null, label: 'Automatic' },
      { kind: 'project', projectUri: selected.candidates[0], label: 'Shop.dproj', description: 'current' },
    ]);
  });

  it('describes the current selection mode and an out-of-catalogue project', () => {
    expect(projectMenuPlaceholder(ambiguous)).toBe('Pascal project (ambiguous)');
    expect(projectMenuPlaceholder({
      ...ambiguous,
      selectedProjectUri: 'file:///home/u/src/App/Other.dproj',
      selectionMode: 'directory',
    })).toBe('Pascal project (directory; current: Other.dproj)');
  });
});

describe('build and installation choices', () => {
  it('puts the build default first and marks the selected candidate', () => {
    expect(buildItems(buildContext.config)).toEqual([
      { value: null, label: 'Automatic (Debug)' },
      { value: 'Debug', label: 'Debug', description: 'current' },
      { value: 'Release', label: 'Release' },
    ]);
    expect(buildItems({ ...buildContext.config, projectDefault: null })[0])
      .toEqual({ value: null, label: 'Automatic (project default)' });
  });

  it('puts Automatic first for installation choices and marks the current one', () => {
    expect(installationItems(installationContext)).toEqual([
      { value: null, label: 'Automatic' },
      { value: 'studio-23', label: 'studio-23', description: 'current' },
      { value: 'studio-22', label: 'studio-22' },
    ]);
    expect(installationSelectionMessage(installationContext))
      .toBe('Delphi installation: studio-23 (configured)');
  });
});

describe('context comparisons', () => {
  it.each([
    ['scopeUri', { scopeUri: 'file:///home/u/other/' }],
    ['selectedProjectUri', { selectedProjectUri: 'file:///home/u/src/App/App.dproj' }],
    ['selectionMode', { selectionMode: 'configured' }],
    ['candidate order', { candidates: [...ambiguous.candidates].reverse() }],
  ])('detects a changed project %s', (_field, change) => {
    expect(sameProjectContext(ambiguous, { ...ambiguous, ...change })).toBe(false);
  });

  it('ignores warning-only project context changes', () => {
    expect(sameProjectContext(ambiguous, { ...ambiguous, warnings: ['warning'] })).toBe(true);
  });

  it.each([
    ['selected', { selected: 'Release' }],
    ['mode', { mode: 'configured' }],
    ['candidate order', { candidates: ['Release', 'Debug'] }],
  ])('detects a changed build %s', (_field, change) => {
    expect(sameBuildField(buildContext.config, { ...buildContext.config, ...change })).toBe(false);
  });

  it('ignores warning-only installation changes but detects selection changes', () => {
    expect(sameInstallationContext(installationContext, {
      ...installationContext,
      warnings: ['warning'],
    })).toBe(true);
    expect(sameInstallationContext(installationContext, {
      ...installationContext,
      selectedInstallationId: 'studio-22',
    })).toBe(false);
    expect(sameInstallationContext(installationContext, {
      ...installationContext,
      selectionMode: 'automatic',
    })).toBe(false);
    expect(sameInstallationContext(installationContext, {
      ...installationContext,
      candidates: [...installationContext.candidates].reverse(),
    })).toBe(false);
  });
});

describe('project candidate and browse decisions', () => {
  it('recognizes catalogue candidates but not Automatic', () => {
    expect(isCandidate(ambiguous, ambiguous.candidates[0]!)).toBe(true);
    expect(isCandidate(ambiguous, null)).toBe(false);
    expect(isCandidate(ambiguous, 'file:///home/u/src/Other/Other.dproj')).toBe(false);
  });

  it('stays in the current project directory', () => {
    expect(browseDestination(
      selected,
      'file:///c%3A/src/Shop/Unit.pas',
      'file:///C:/src/shop/Other.dproj',
      selected,
    )).toEqual({ kind: 'stay' });
  });

  it('checks the selected project main source when browsing another directory', () => {
    expect(browseDestination(
      selected,
      'file:///c%3A/src/Shop/Unit.pas',
      'file:///c%3A/src/Other/Other.dproj',
      selected,
    )).toEqual({ kind: 'check-main', mainSourceUri: selected.mainSourceUri });
  });

  it('opens the project when it has no usable main source', () => {
    expect(browseDestination(
      selected,
      'file:///c%3A/src/Shop/Unit.pas',
      'file:///c%3A/src/Other/Other.dproj',
      { ...selected, mainSourceUri: null },
    )).toEqual({
      kind: 'open-project',
      reason: 'Selected project has no usable main source; opening the project file.',
    });
  });

  it('uses the document directory if the previous context is Automatic', () => {
    expect(browseDestination(
      ambiguous,
      'file:///home/u/src/App/Unit.pas',
      'file:///home/u/src/App/New.dproj',
      selected,
    )).toEqual({ kind: 'stay' });
  });

  it('confirms a main source only when it is the selected project URI', () => {
    expect(mainSourceConfirmed(selected, selected.selectedProjectUri!)).toBe(true);
    expect(mainSourceConfirmed(selected, 'file:///c%3A/src/Other/Other.dproj')).toBe(false);
  });
});

describe('selection messages and warning summaries', () => {
  it('reports build selection and open conditional reasons', () => {
    expect(buildSelectionMessage('config', buildContext))
      .toBe('Build configuration: Debug (projectDefault); conditionals open: compilerVersion unknown');
    expect(buildSelectionMessage('config', {
      ...buildContext,
      conditionals: { ...buildContext.conditionals, closed: true },
    })).toBe('Build configuration: Debug (projectDefault); conditionals closed');
    expect(buildSelectionMessage('config', {
      ...buildContext,
      config: { ...buildContext.config, selected: null },
    })).toBe('Build configuration: Automatic; conditionals open: compilerVersion unknown');
  });

  it('summarizes zero, one, and multiple warnings', () => {
    expect(summarizeWarnings([])).toBeUndefined();
    expect(summarizeWarnings(['a'])).toBe('a');
    expect(summarizeWarnings(['a', 'b', 'c'])).toBe('a (+2 more)');
  });
});

describe('Generation', () => {
  it('increments and identifies only the current generation', () => {
    const generation = new Generation();
    const first = generation.next();
    expect(generation.current).toBe(first);
    const second = generation.next();
    expect(second).toBe(first + 1);
    expect(generation.isCurrent(first)).toBe(false);
    expect(generation.isCurrent(second)).toBe(true);
  });
});

describe('SerialQueue', () => {
  it('waits for the first job to settle before starting the second', async () => {
    const queue = new SerialQueue();
    const order: string[] = [];
    let release!: () => void;
    const gate = new Promise<void>((resolveGate) => { release = resolveGate; });
    const first = queue.run(async () => {
      order.push('first:start');
      await gate;
      order.push('first:end');
    });
    const second = queue.run(async () => { order.push('second'); });

    await Promise.resolve();
    expect(order).toEqual(['first:start']);
    release();
    await Promise.all([first, second]);
    expect(order).toEqual(['first:start', 'first:end', 'second']);
  });

  it('continues after a rejected job while preserving its rejection', async () => {
    const queue = new SerialQueue();
    const order: string[] = [];
    const failure = queue.run(async () => { throw new Error('expected failure'); });
    const afterFailure = queue.run(async () => { order.push('after failure'); });

    await expect(failure).rejects.toThrow('expected failure');
    await afterFailure;
    expect(order).toEqual(['after failure']);
  });
});
