import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import type {
  BuildContext,
  CacheStatus,
  IndexStatus,
  InstallationContext,
  ProjectContext,
} from '../../src/protocol';
import {
  FAST_POLL_MS,
  pollInterval,
  SLOW_POLL_MS,
  statusIcon,
  statusText,
  statusTooltip,
  type StatusSnapshot,
  fileName,
} from '../../src/statusFormat';

function fixture<T>(name: string): T {
  return JSON.parse(readFileSync(resolve('test/fixtures/protocol', name), 'utf8')) as T;
}

const selected = fixture<ProjectContext>('projectContext.selected.json');
const ambiguous = fixture<ProjectContext>('projectContext.ambiguous.json');
const build = fixture<BuildContext>('buildContext.json');
const installation = fixture<InstallationContext>('installationContext.json');
const pendingIndex = fixture<IndexStatus>('indexStatus.pending.json');
const completeIndex = fixture<IndexStatus>('indexStatus.complete.json');
const bypassedCache = fixture<CacheStatus>('cacheStatus.bypassed.json');
const enabledCache = fixture<CacheStatus>('cacheStatus.enabled.json');

function snapshot(overrides: Partial<StatusSnapshot> = {}): StatusSnapshot {
  return { clientStatus: 'running', stale: false, ...overrides };
}

describe('status text', () => {
  it('uses the status icon before the namespace icon and formats a selected project and build', () => {
    expect(statusText(snapshot({ project: selected, build })))
      .toBe('$(symbol-namespace) Shop.dproj · Debug/Win32');
  });

  it('shows a warning for an ambiguous project and names standalone mode', () => {
    expect(statusText(snapshot({ project: ambiguous })))
      .toBe('$(warning) $(symbol-namespace) Pascal (ambiguous)');
    expect(statusText(snapshot({ project: {
      ...selected,
      candidates: [],
      selectedProjectUri: null,
      selectionMode: 'standalone',
    } })))
      .toBe('$(symbol-namespace) Pascal (standalone)');
  });

  it('shows Pascal before context arrives and includes only known build fields', () => {
    expect(statusText(snapshot())).toBe('$(symbol-namespace) Pascal');
    expect(statusText(snapshot({
      project: selected,
      build: { ...build, platform: { ...build.platform, selected: null } },
    }))).toBe('$(symbol-namespace) Shop.dproj · Debug');
  });

  it('applies error, spin, and warning prefixes in priority order', () => {
    expect(statusText(snapshot({ clientStatus: 'failed' }))).toMatch(/^\$\(error\) /);
    expect(statusText(snapshot({ clientStatus: 'starting' }))).toMatch(/^\$\(sync~spin\) /);
    expect(statusText(snapshot({ index: pendingIndex }))).toMatch(/^\$\(sync~spin\) /);
    expect(statusText(snapshot({ cache: bypassedCache }))).toMatch(/^\$\(warning\) /);
    expect(statusText(snapshot({
      clientStatus: 'failed', index: pendingIndex, cache: bypassedCache,
    }))).toMatch(/^\$\(error\) /);
    expect(statusText(snapshot({
      clientStatus: 'starting', index: pendingIndex, cache: bypassedCache,
    }))).toMatch(/^\$\(sync~spin\) /);
  });

  it('warns on invalid project and build modes', () => {
    expect(statusIcon(snapshot({ project: { ...selected, selectionMode: 'invalid' } })))
      .toBe('$(warning) ');
    expect(statusIcon(snapshot({ build: {
      ...build,
      config: { ...build.config, mode: 'invalid' },
    } }))).toBe('$(warning) ');
  });
});

describe('status tooltip', () => {
  it('includes project, build, conditionals, installation, latest index, and cache details', () => {
    const tooltip = statusTooltip(snapshot({
      project: selected,
      build,
      installation,
      index: pendingIndex,
      cache: bypassedCache,
    }));

    expect(tooltip).toContain('**Project:** Shop.dproj (directory)');
    expect(tooltip).toContain('Configuration: Debug (projectDefault)');
    expect(tooltip).toContain('Platform: Win32 (configured)');
    expect(tooltip).toContain('Conditionals: open (compilerVersion unknown)');
    expect(tooltip).toContain('Delphi installation: studio-23 (configured)');
    expect(tooltip).toContain('Index: fresh 812 / 840 members, 28 pending');
    expect(tooltip).toContain('Cache: bypassed (lowDiskSpace)');
    expect(tooltip).toContain('`pascal-lsp --cache-prune`');
  });

  it('uses cache reset advice and omits cause details and commands when there is no cause', () => {
    const reset = statusTooltip(snapshot({
      cache: { ...bypassedCache, cause: { kind: 'namespaceMismatch', advice: 'resetNamespace' } },
    }));
    expect(reset).toContain('`pascal-lsp --cache-reset`');

    const enabled = statusTooltip(snapshot({ cache: enabledCache }));
    expect(enabled).toContain('Cache: enabled');
    expect(enabled).not.toContain('Cache: enabled (');
    expect(enabled).not.toContain('pascal-lsp --cache-');
  });

  it('handles an empty index context list and marks previous values stale', () => {
    const tooltip = statusTooltip(snapshot({
      index: { ...completeIndex, contexts: [] },
      stale: true,
    }));
    expect(tooltip).toContain('Index: no contexts yet');
    expect(tooltip).toContain('_Last refresh failed; showing previous values._');
  });

  it('includes non-zero index refused, limited, and retrying counts only', () => {
    const context = pendingIndex.contexts[0]!;
    const tooltip = statusTooltip(snapshot({
      index: { ...pendingIndex, contexts: [{ ...context, refused: 2, limited: 3, retrying: 4 }] },
    }));
    expect(tooltip).toContain('Index: fresh 812 / 840 members, 28 pending, 2 refused, 3 limited, 4 retrying');
  });

  it('never renders undefined or null for fixture combinations', () => {
    const contexts: (ProjectContext | undefined)[] = [undefined, selected, ambiguous];
    const builds: (BuildContext | undefined)[] = [undefined, build];
    const installations: (InstallationContext | undefined)[] = [undefined, installation];
    const indexes: (IndexStatus | undefined)[] = [
      undefined,
      pendingIndex,
      completeIndex,
      { ...completeIndex, contexts: [] },
    ];
    const caches: (CacheStatus | undefined)[] = [undefined, enabledCache, bypassedCache];

    for (const project of contexts) {
      for (const buildContext of builds) {
        for (const installationContext of installations) {
          for (const index of indexes) {
            for (const cache of caches) {
              expect(statusTooltip(snapshot({
                project,
                build: buildContext,
                installation: installationContext,
                index,
                cache,
              }))).not.toMatch(/\b(undefined|null)\b/);
            }
          }
        }
      }
    }
  });
});

describe('polling and URI labels', () => {
  it('polls quickly until all index contexts are complete', () => {
    expect(FAST_POLL_MS).toBe(5000);
    expect(SLOW_POLL_MS).toBe(30000);
    expect(pollInterval(undefined)).toBe(FAST_POLL_MS);
    expect(pollInterval(pendingIndex)).toBe(FAST_POLL_MS);
    expect(pollInterval({ ...completeIndex, contexts: [] })).toBe(FAST_POLL_MS);
    expect(pollInterval(completeIndex)).toBe(SLOW_POLL_MS);
  });

  it('extracts and decodes file names from URIs', () => {
    expect(fileName('file:///c%3A/src/Shop/Shop%20App.dproj')).toBe('Shop App.dproj');
    expect(fileName('file:///home/u/src/App/Unit.pas')).toBe('Unit.pas');
  });
});
