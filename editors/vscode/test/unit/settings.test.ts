import { describe, expect, it } from 'vitest';
import {
  affectsServerSettings,
  buildInitializationOptions,
  buildServerSettings,
  isSet,
  RESTART_KEYS,
  requiresRestart,
  RUNTIME_FIELDS,
  STARTUP_ONLY_FIELDS,
} from '../../src/settings';

const reader = (values: Record<string, unknown>) => (key: string) => values[key];

describe('settings fields', () => {
  it('exports the runtime and startup-only server fields', () => {
    expect(RUNTIME_FIELDS).toEqual([
      'projectFile', 'buildConfig', 'platform', 'sourcePaths', 'exclude',
      'compilerVersion', 'compilerOptions', 'conditionalDefines', 'conditionalUndefines', 'conditionalConstants',
      'maxFiles', 'maxFileBytes', 'maxTotalBytes', 'maxCacheBytes', 'maxWorkspaceUnits',
      'persistentCacheEnabled', 'persistentCachePageBytes', 'persistentCacheScratchBytes', 'persistentCacheWaitMs',
    ]);
    expect(STARTUP_ONLY_FIELDS).toEqual(['persistentCacheDirectory', 'persistentCacheMaxNamespaces']);
    expect(RESTART_KEYS).toEqual([
      'serverPath', 'serverArgs', 'serverEnv', 'persistentCacheDirectory', 'persistentCacheMaxNamespaces',
    ]);
  });
});

describe('isSet', () => {
  it('treats only undefined, null and the empty string as unset', () => {
    expect(isSet(undefined)).toBe(false);
    expect(isSet(null)).toBe(false);
    expect(isSet('')).toBe(false);
    expect(isSet(false)).toBe(true);
    expect(isSet(0)).toBe(true);
    expect(isSet([])).toBe(true);
    expect(isSet(' ')).toBe(true);
  });
});

describe('buildServerSettings', () => {
  it('returns only set runtime server fields, unchanged', () => {
    expect(buildServerSettings(reader({
      sourcePaths: ['C:/DelphiSources/rtl/sys'],
      maxFiles: 5,
      serverPath: '/x',
      persistentCacheDirectory: '/c',
    }))).toEqual({ sourcePaths: ['C:/DelphiSources/rtl/sys'], maxFiles: 5 });
  });

  it('omits null, undefined and empty strings', () => {
    expect(buildServerSettings(reader({ sourcePaths: null, projectFile: '', buildConfig: undefined }))).toEqual({});
  });

  it('keeps falsy but explicit non-string values', () => {
    expect(buildServerSettings(reader({ persistentCacheEnabled: false, exclude: [], maxFiles: 0 })))
      .toEqual({ persistentCacheEnabled: false, exclude: [], maxFiles: 0 });
  });

  it('never emits null in initialization options', () => {
    const out = buildInitializationOptions(reader({ sourcePaths: null, persistentCacheDirectory: null }));
    expect(Object.values(out)).not.toContain(null);
  });
});

describe('buildInitializationOptions', () => {
  it('adds startup-only fields to runtime fields', () => {
    expect(buildInitializationOptions(reader({
      platform: 'Win32',
      persistentCacheDirectory: '/c',
      persistentCacheMaxNamespaces: 1,
    }))).toEqual({ platform: 'Win32', persistentCacheDirectory: '/c', persistentCacheMaxNamespaces: 1 });
  });
});

describe('change classification', () => {
  const affects = (keys: string[]) => (key: string) => keys.includes(key);

  it('requires restart for server startup keys only', () => {
    expect(requiresRestart(affects(['pascalLsp.serverPath']))).toBe(true);
    expect(requiresRestart(affects(['pascalLsp.serverArgs']))).toBe(true);
    expect(requiresRestart(affects(['pascalLsp.serverEnv']))).toBe(true);
    expect(requiresRestart(affects(['pascalLsp.persistentCacheDirectory']))).toBe(true);
    expect(requiresRestart(affects(['pascalLsp.persistentCacheMaxNamespaces']))).toBe(true);
    expect(requiresRestart(affects(['pascalLsp.sourcePaths']))).toBe(false);
  });

  it('detects runtime server settings but not client-only settings', () => {
    expect(affectsServerSettings(affects(['pascalLsp.sourcePaths']))).toBe(true);
    expect(affectsServerSettings(affects(['pascalLsp.statusBar.enabled']))).toBe(false);
    expect(affectsServerSettings(affects(['pascalLsp.trace.server']))).toBe(false);
  });
});
