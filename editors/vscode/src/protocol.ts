export const EXPERIMENTAL_FLAGS = [
  'projectSelection',
  'projectCatalogue',
  'buildSelection',
  'installationSelection',
] as const;

export type ExperimentalFlag = (typeof EXPERIMENTAL_FLAGS)[number];

export const Methods = {
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
} as const;

export interface ProjectContext {
  scopeUri: string | null;
  candidates: string[];
  selectedProjectUri: string | null;
  selectionMode: string;
  selectedInstallationId: string | null;
  installationSelectionMode: string;
  installationCandidates: string[];
  mainSourceUri: string | null;
  installationConfigUris: string[];
  pathIssues: unknown[];
  lintConfigUri: string | null;
  fmtConfigUri: string | null;
  warnings: string[];
}

export interface InstallationContext {
  projectUri: string;
  candidates: string[];
  selectedInstallationId: string | null;
  selectionMode: string;
  warnings: string[];
}

export interface BuildSelectionInfo {
  selected: string | null;
  candidates: string[];
  mode: string;
  projectDefault: string | null;
}

export interface BuildContext {
  projectUri: string;
  config: BuildSelectionInfo;
  platform: BuildSelectionInfo;
  conditionals: {
    closed: boolean;
    openReasons: string[];
    rtlVersionConstants: { source: string; names: string[] };
  };
  warnings: string[];
}

export interface ProjectCatalogue {
  projects: { projectUri: string; label: string }[];
  complete: boolean;
  warnings: string[];
}

export interface IndexContext {
  anchor: { document: string; project?: never } | { project: string; document?: never };
  anchorTruncated: boolean;
  members: number;
  fresh: number;
  reused: number;
  extracted: number;
  persisted: number;
  pending: number;
  reusePending: number;
  refused: number;
  limited: number;
  deferred: number;
  retrying: number;
  overLimit: number;
  unprovable: number;
  overDeadline: number;
  overAdmission: number;
  membershipComplete: boolean;
  complete: boolean;
  snapshot: string;
  validationReads: number;
  publicationsRefused: number;
  gaps: string[];
  [extra: string]: unknown;
}

export interface IndexStatus {
  schema: number;
  crawlPublished: number;
  crawlDeferred: number;
  providerGraphsRefused: number;
  contexts: IndexContext[];
}

export interface CacheCause {
  kind: string;
  advice: string;
  message?: string;
  location?: string | null;
}

export interface CacheStore {
  databaseBytes: number;
  walBytes: number;
  shmBytes: number;
  freeBytes: number;
  filesystemFreeBytes: number | null;
  maxDiskBytes: number;
  walThresholdBytes: number;
  policyRevision: number;
  pressure: string;
}

export interface CacheStatus {
  schema: number;
  state: string;
  generation: number;
  enabled: boolean;
  cause: CacheCause | null;
  directory: string | null;
  limits: { pageBytes: number; scratchBytes: number; waitMs: number; maxNamespaces: number };
  resetEpoch: number | null;
  reservedBytes: {
    pages: number;
    scratch: number;
    facts: number;
    publication: number;
    queryTrees: number;
  };
  crawlBytes: { leased: number; allowance: number; overBudget: number; maxOvershoot: number };
  store: CacheStore | null;
  lockWaits: {
    poolWaits: number;
    poolWaitMicros: number;
    lockWaits: number;
    lockWaitMicros: number;
    busy: number;
  };
  counters: {
    hits: number;
    decodedBytes: number;
    misses: Record<string, number>;
    bypasses: Record<string, number>;
    maintenance: { steps: number; cycles: number; rows: number; skipped: number };
  };
  [extra: string]: unknown;
}

export type BuildField = 'config' | 'platform';

export function capabilityFlags(experimental: unknown): Record<ExperimentalFlag, boolean> {
  const source = typeof experimental === 'object' && experimental !== null
    ? experimental as Record<string, unknown>
    : {};
  return Object.fromEntries(EXPERIMENTAL_FLAGS.map((flag) => [flag, source[flag] === true])) as Record<
    ExperimentalFlag,
    boolean
  >;
}
