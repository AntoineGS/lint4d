export const RUNTIME_FIELDS = [
  'projectFile', 'buildConfig', 'platform', 'sourcePaths', 'exclude',
  'compilerVersion', 'compilerOptions', 'conditionalDefines', 'conditionalUndefines', 'conditionalConstants',
  'maxFiles', 'maxFileBytes', 'maxTotalBytes', 'maxCacheBytes', 'maxWorkspaceUnits',
  'persistentCacheEnabled', 'persistentCachePageBytes', 'persistentCacheScratchBytes', 'persistentCacheWaitMs',
] as const;

export const STARTUP_ONLY_FIELDS = ['persistentCacheDirectory', 'persistentCacheMaxNamespaces'] as const;

export const RESTART_KEYS = ['serverPath', 'serverArgs', 'serverEnv', ...STARTUP_ONLY_FIELDS] as const;

export type ConfigReader = (key: string) => unknown;

export function isSet(value: unknown): boolean {
  return value !== undefined && value !== null && value !== '';
}

function pick(read: ConfigReader, fields: readonly string[]): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const field of fields) {
    const value = read(field);
    if (isSet(value)) out[field] = value;
  }
  return out;
}

export function buildServerSettings(read: ConfigReader): Record<string, unknown> {
  return pick(read, RUNTIME_FIELDS);
}

export function buildInitializationOptions(read: ConfigReader): Record<string, unknown> {
  return { ...pick(read, RUNTIME_FIELDS), ...pick(read, STARTUP_ONLY_FIELDS) };
}

export function requiresRestart(affects: (fullKey: string) => boolean): boolean {
  return RESTART_KEYS.some((key) => affects(`pascalLsp.${key}`));
}

export function affectsServerSettings(affects: (fullKey: string) => boolean): boolean {
  return RUNTIME_FIELDS.some((key) => affects(`pascalLsp.${key}`));
}
