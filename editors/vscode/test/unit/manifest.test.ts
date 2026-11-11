import { describe, expect, it } from 'vitest';
import manifest from '../../package.json';

const SERVER_FIELDS = [
  'projectFile', 'buildConfig', 'platform', 'sourcePaths', 'exclude',
  'compilerVersion', 'compilerOptions', 'conditionalDefines', 'conditionalUndefines', 'conditionalConstants',
  'maxFiles', 'maxFileBytes', 'maxTotalBytes', 'maxCacheBytes', 'maxWorkspaceUnits',
  'persistentCacheEnabled', 'persistentCachePageBytes', 'persistentCacheScratchBytes', 'persistentCacheWaitMs',
  'persistentCacheDirectory', 'persistentCacheMaxNamespaces',
];

describe('manifest', () => {
  const props = manifest.contributes.configuration.properties as Record<string, { default?: unknown }>;
  it('declares every server field without a default', () => {
    for (const field of SERVER_FIELDS) {
      const prop = props[`pascalLsp.${field}`];
      expect(prop, field).toBeDefined();
      expect(prop && 'default' in prop, `${field} must not declare a default`).toBe(false);
    }
  });
  it('declares every command once with the Pascal category', () => {
    const ids = manifest.contributes.commands.map((c) => c.command);
    expect(new Set(ids).size).toBe(ids.length);
    expect(ids.sort()).toEqual([
      'pascal.browseProjects', 'pascal.projectMenu', 'pascal.refreshStatus', 'pascal.restartServer',
      'pascal.selectBuildConfig', 'pascal.selectInstallation', 'pascal.selectPlatform', 'pascal.showOutput', 'pascal.showStatus',
    ]);
    for (const c of manifest.contributes.commands) expect(c.category).toBe('Pascal');
  });
  it('refuses untrusted workspaces', () => {
    expect(manifest.capabilities.untrustedWorkspaces.supported).toBe(false);
  });
});
