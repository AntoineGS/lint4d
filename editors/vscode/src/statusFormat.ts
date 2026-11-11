import type {
  BuildContext,
  CacheStatus,
  IndexStatus,
  InstallationContext,
  ProjectContext,
} from './protocol';

export interface StatusSnapshot {
  clientStatus: 'stopped' | 'starting' | 'running' | 'failed';
  project?: ProjectContext;
  build?: BuildContext;
  installation?: InstallationContext;
  index?: IndexStatus;
  cache?: CacheStatus;
  stale: boolean;
}

export const FAST_POLL_MS = 5000;
export const SLOW_POLL_MS = 30000;

export function statusIcon(snapshot: StatusSnapshot): '' | '$(error) ' | '$(sync~spin) ' | '$(warning) ' {
  if (snapshot.clientStatus === 'stopped' || snapshot.clientStatus === 'failed') return '$(error) ';
  if (snapshot.clientStatus === 'starting' || snapshot.index?.contexts.some((context) => context.pending > 0)) {
    return '$(sync~spin) ';
  }
  if (
    snapshot.cache?.state === 'bypassed'
    || snapshot.project?.selectionMode === 'invalid'
    || snapshot.project?.selectionMode === 'ambiguous'
    || snapshot.build?.config.mode === 'invalid'
    || snapshot.build?.platform.mode === 'invalid'
  ) {
    return '$(warning) ';
  }
  return '';
}

export function statusText(snapshot: StatusSnapshot): string {
  let label = 'Pascal';
  const projectUri = snapshot.project?.selectedProjectUri;
  if (typeof projectUri === 'string') {
    const projectName = fileName(projectUri);
    const buildFields = [snapshot.build?.config.selected, snapshot.build?.platform.selected]
      .filter((value): value is string => typeof value === 'string' && value.length > 0);
    label = buildFields.length > 0 ? `${projectName} · ${buildFields.join('/')}` : projectName;
  } else if (snapshot.project?.selectionMode) {
    label = `Pascal (${snapshot.project.selectionMode})`;
  }

  return `${statusIcon(snapshot)}$(symbol-namespace) ${label}`;
}

export function statusTooltip(snapshot: StatusSnapshot): string {
  const lines: string[] = [];
  const project = snapshot.project;
  if (project) {
    const label = typeof project.selectedProjectUri === 'string'
      ? fileName(project.selectedProjectUri)
      : 'Pascal';
    lines.push(`**Project:** ${label}${project.selectionMode ? ` (${project.selectionMode})` : ''}`);
  }

  if (snapshot.build) {
    lines.push(selectionLine('Configuration', snapshot.build.config.selected, snapshot.build.config.mode));
    lines.push(selectionLine('Platform', snapshot.build.platform.selected, snapshot.build.platform.mode));
    if (snapshot.build.conditionals.closed) {
      lines.push('Conditionals: closed');
    } else {
      const reasons = snapshot.build.conditionals.openReasons;
      lines.push(`Conditionals: open${reasons.length > 0 ? ` (${reasons.join(', ')})` : ''}`);
    }
  }

  if (snapshot.installation) {
    lines.push(selectionLine(
      'Delphi installation',
      snapshot.installation.selectedInstallationId,
      snapshot.installation.selectionMode,
    ));
  }

  if (snapshot.index) lines.push(indexLine(snapshot.index));
  if (snapshot.cache) lines.push(cacheLine(snapshot.cache));
  if (snapshot.stale) lines.push('_Last refresh failed; showing previous values._');
  return lines.join('\n');
}

export function pollInterval(index: IndexStatus | undefined): number {
  if (!index || index.contexts.length === 0 || index.contexts.some((context) => context.pending > 0)) {
    return FAST_POLL_MS;
  }
  return SLOW_POLL_MS;
}

export function fileName(uri: string): string {
  try {
    const pathname = decodeURIComponent(new URL(uri).pathname);
    const segments = pathname.split(/[\\/]/).filter(Boolean);
    return segments.at(-1) ?? uri;
  } catch {
    const path = uri.split(/[?#]/, 1)[0] ?? uri;
    const segment = path.split(/[\\/]/).filter(Boolean).at(-1);
    if (!segment) return uri;
    try {
      return decodeURIComponent(segment);
    } catch {
      return segment;
    }
  }
}

function selectionLine(label: string, selected: string | null, mode: string): string {
  const value = typeof selected === 'string' && selected.length > 0 ? selected : 'Automatic';
  return `${label}: ${value}${mode ? ` (${mode})` : ''}`;
}

function count(value: number): number {
  return Number.isFinite(value) ? value : 0;
}

function indexLine(index: IndexStatus): string {
  const context = index.contexts[0];
  if (!context) return 'Index: no contexts yet';

  const details = [
    ['refused', context.refused],
    ['limited', context.limited],
    ['retrying', context.retrying],
  ] as const;
  const additional = details
    .filter(([, value]) => count(value) !== 0)
    .map(([label, value]) => `${count(value)} ${label}`);
  const suffix = additional.length > 0 ? `, ${additional.join(', ')}` : '';
  return `Index: fresh ${count(context.fresh)} / ${count(context.members)} members, ${count(context.pending)} pending${suffix}`;
}

function cacheLine(cache: CacheStatus): string {
  const cause = cache.cause;
  const causeLabel = cause && typeof cause.kind === 'string' && cause.kind.length > 0
    ? ` (${cause.kind})`
    : '';
  const lines = [`Cache: ${cache.state}${causeLabel}`];
  if (cause?.advice === 'prune') lines.push('Run `pascal-lsp --cache-prune`.');
  if (cause?.advice === 'resetNamespace') lines.push('Run `pascal-lsp --cache-reset`.');
  return lines.join('\n');
}
