import path from 'node:path';
import { URI } from 'vscode-uri';
import type {
  BuildContext,
  BuildField,
  BuildSelectionInfo,
  ExperimentalFlag,
  InstallationContext,
  ProjectContext,
} from './protocol';

export type MenuItem =
  | { kind: 'project'; projectUri: string | null; label: string; description?: string }
  | { kind: 'browse' | 'installation' | 'build-config' | 'platform'; label: string };

export interface ValueItem {
  value: string | null;
  label: string;
  description?: string;
}

function isWindowsDrivePath(uriPath: string): boolean {
  return /^\/[A-Za-z]:/.test(uriPath);
}

export function directoryKey(uri: string): string {
  const directory = path.posix.dirname(URI.parse(uri).path);
  return isWindowsDrivePath(directory) ? directory.toLowerCase() : directory;
}

export function relativeLabel(projectUri: string, scopeUri: string | null): string {
  const projectPath = URI.parse(projectUri).path;
  const projectName = path.posix.basename(projectPath);
  if (scopeUri === null) return projectName;

  const scopePath = URI.parse(scopeUri).path;
  const projectIsDrivePath = isWindowsDrivePath(projectPath);
  const scopeIsDrivePath = isWindowsDrivePath(scopePath);
  if (projectIsDrivePath !== scopeIsDrivePath) return projectName;

  if (projectIsDrivePath && scopeIsDrivePath) {
    const comparable = path.posix.relative(scopePath.toLowerCase(), projectPath.toLowerCase());
    if (!comparable || comparable.startsWith('..')) return projectName;

    // The lowercased relative path proves containment; segment slicing keeps
    // the spelling of the candidate URI in the label.
    const scopeSegments = path.posix.resolve(scopePath).split('/').filter(Boolean);
    const projectSegments = path.posix.resolve(projectPath).split('/').filter(Boolean);
    return projectSegments.slice(scopeSegments.length).join('/') || projectName;
  }

  const relative = path.posix.relative(scopePath, projectPath);
  return !relative || relative.startsWith('..') ? projectName : relative;
}

export function projectMenuItems(
  ctx: ProjectContext,
  flags: Record<ExperimentalFlag, boolean>,
): MenuItem[] {
  const items: MenuItem[] = [
    { kind: 'project', projectUri: null, label: 'Automatic' },
    ...ctx.candidates.map((projectUri): MenuItem => ({
      kind: 'project',
      projectUri,
      label: relativeLabel(projectUri, ctx.scopeUri),
      ...(projectUri === ctx.selectedProjectUri ? { description: 'current' } : {}),
    })),
  ];

  if (flags.projectCatalogue) items.push({ kind: 'browse', label: 'Browse for project…' });
  if (flags.installationSelection) {
    items.push({ kind: 'installation', label: 'Select Delphi installation…' });
  }
  if (flags.buildSelection) {
    items.push({ kind: 'build-config', label: 'Select build configuration…' });
    items.push({ kind: 'platform', label: 'Select platform…' });
  }
  return items;
}

export function projectMenuPlaceholder(ctx: ProjectContext): string {
  if (ctx.selectedProjectUri !== null && !ctx.candidates.includes(ctx.selectedProjectUri)) {
    const current = path.posix.basename(URI.parse(ctx.selectedProjectUri).path);
    return `Pascal project (${ctx.selectionMode}; current: ${current})`;
  }
  return `Pascal project (${ctx.selectionMode})`;
}

export function buildItems(info: BuildSelectionInfo): ValueItem[] {
  return [
    { value: null, label: `Automatic (${info.projectDefault ?? 'project default'})` },
    ...info.candidates.map((value): ValueItem => ({
      value,
      label: value,
      ...(value === info.selected ? { description: 'current' } : {}),
    })),
  ];
}

export function installationItems(ctx: InstallationContext): ValueItem[] {
  return [
    { value: null, label: 'Automatic' },
    ...ctx.candidates.map((value): ValueItem => ({
      value,
      label: value,
      ...(value === ctx.selectedInstallationId ? { description: 'current' } : {}),
    })),
  ];
}

function sameItems<T>(a: readonly T[], b: readonly T[]): boolean {
  return a.length === b.length && a.every((item, index) => item === b[index]);
}

export function sameProjectContext(a: ProjectContext, b: ProjectContext): boolean {
  return a.scopeUri === b.scopeUri
    && a.selectedProjectUri === b.selectedProjectUri
    && a.selectionMode === b.selectionMode
    && sameItems(a.candidates, b.candidates);
}

export function sameBuildField(a: BuildSelectionInfo, b: BuildSelectionInfo): boolean {
  return a.selected === b.selected
    && a.mode === b.mode
    && a.projectDefault === b.projectDefault
    && sameItems(a.candidates, b.candidates);
}

export function sameInstallationContext(a: InstallationContext, b: InstallationContext): boolean {
  return a.projectUri === b.projectUri
    && a.selectedInstallationId === b.selectedInstallationId
    && a.selectionMode === b.selectionMode
    && sameItems(a.candidates, b.candidates);
}

export function isCandidate(ctx: ProjectContext, projectUri: string | null): boolean {
  return projectUri !== null && ctx.candidates.includes(projectUri);
}

export type BrowseDestination =
  | { kind: 'stay' }
  | { kind: 'check-main'; mainSourceUri: string }
  | { kind: 'open-project'; reason: string };

export function browseDestination(
  previous: ProjectContext,
  documentUri: string,
  targetUri: string,
  selected: ProjectContext,
): BrowseDestination {
  const previousDirectory = previous.selectedProjectUri === null
    ? directoryKey(documentUri)
    : directoryKey(previous.selectedProjectUri);
  if (previousDirectory === directoryKey(targetUri)) return { kind: 'stay' };
  if (selected.mainSourceUri !== null) {
    return { kind: 'check-main', mainSourceUri: selected.mainSourceUri };
  }
  return {
    kind: 'open-project',
    reason: 'Selected project has no usable main source; opening the project file.',
  };
}

export function mainSourceConfirmed(sourceContext: ProjectContext, targetUri: string): boolean {
  return sourceContext.selectedProjectUri === targetUri;
}

export function createBrowseNavigationGuard(
  documentUri: string,
): (activeDocumentUri: string | undefined) => boolean {
  return (activeDocumentUri) => activeDocumentUri === documentUri;
}

export function buildSelectionMessage(field: BuildField, ctx: BuildContext): string {
  const info = ctx[field];
  const name = field === 'config' ? 'Build configuration' : 'Platform';
  const value = `${info.selected ?? 'Automatic'} (${info.mode})`;
  const conditionals = ctx.conditionals.closed
    ? 'conditionals closed'
    : `conditionals open: ${ctx.conditionals.openReasons.join(', ')}`;
  return `${name}: ${value}; ${conditionals}`;
}

export function installationSelectionMessage(ctx: InstallationContext): string {
  const value = `${ctx.selectedInstallationId ?? 'Automatic'} (${ctx.selectionMode})`;
  return `Delphi installation selection: ${value}`;
}

export function summarizeWarnings(warnings: readonly string[]): string | undefined {
  if (warnings.length === 0) return undefined;
  return warnings.length === 1 ? warnings[0] : `${warnings[0]} (+${warnings.length - 1} more)`;
}

export class Generation {
  private value = 0;

  get current(): number {
    return this.value;
  }

  next(): number {
    this.value += 1;
    return this.value;
  }

  isCurrent(n: number): boolean {
    return this.value === n;
  }
}

export class SerialQueue {
  private tail: Promise<void> = Promise.resolve();

  run<T>(job: () => Promise<T>): Promise<T> {
    const result = this.tail.then(job);
    this.tail = result.then(() => undefined, () => undefined);
    return result;
  }
}
