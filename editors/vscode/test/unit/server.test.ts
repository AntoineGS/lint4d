import { describe, expect, it } from 'vitest';
import {
  BUILD_HINT,
  expandPath,
  resolveServerBinary,
  resolutionMessage,
  type ResolveEnv,
} from '../../src/server';

function makeEnv(overrides: Partial<ResolveEnv> = {}): ResolveEnv {
  return {
    platform: 'linux',
    env: {},
    homedir: '/home/u',
    workspaceFolder: undefined,
    isFile: () => false,
    ...overrides,
  };
}

describe('expandPath', () => {
  it('expands a POSIX home-relative path', () => {
    expect(expandPath('~/gits/lint4d/target/release/pascal-lsp', makeEnv()))
      .toBe('/home/u/gits/lint4d/target/release/pascal-lsp');
  });

  it('expands a Windows home-relative path with Windows separators', () => {
    expect(expandPath('~\\bin\\pascal-lsp.exe', makeEnv({
      platform: 'win32',
      homedir: 'C:\\Users\\u',
    }))).toBe('C:\\Users\\u\\bin\\pascal-lsp.exe');
  });

  it('expands the workspace folder', () => {
    expect(expandPath('${workspaceFolder}/bin/pascal-lsp', makeEnv({ workspaceFolder: '/work/project' })))
      .toBe('/work/project/bin/pascal-lsp');
  });

  it('leaves an unresolved workspace folder literal', () => {
    expect(expandPath('${workspaceFolder}/bin/pascal-lsp', makeEnv()))
      .toBe('${workspaceFolder}/bin/pascal-lsp');
  });
});

describe('resolveServerBinary setting', () => {
  it('resolves an existing configured file', () => {
    const result = resolveServerBinary('/custom/pascal-lsp', makeEnv({
      isFile: (candidate) => candidate === '/custom/pascal-lsp',
    }));

    expect(result).toEqual({ ok: true, path: '/custom/pascal-lsp', source: 'setting' });
  });

  it('reports a missing configured file without consulting PATH', () => {
    const checked: string[] = [];
    const result = resolveServerBinary('/custom/pascal-lsp', makeEnv({
      env: { PATH: '/bin' },
      isFile: (candidate) => {
        checked.push(candidate);
        return candidate === '/bin/pascal-lsp';
      },
    }));

    expect(result).toEqual({ ok: false, reason: 'missing-setting', path: '/custom/pascal-lsp' });
    expect(checked).toEqual(['/custom/pascal-lsp']);
  });

  it('treats an empty configured path as unset', () => {
    const result = resolveServerBinary('', makeEnv({
      env: { PATH: '/bin' },
      isFile: (candidate) => candidate === '/bin/pascal-lsp',
    }));

    expect(result).toEqual({ ok: true, path: '/bin/pascal-lsp', source: 'path' });
  });

  it('reports a literal unresolved workspace folder path as missing', () => {
    expect(resolveServerBinary('${workspaceFolder}', makeEnv()))
      .toEqual({ ok: false, reason: 'missing-setting', path: '${workspaceFolder}' });
  });
});

describe('resolveServerBinary environment variable', () => {
  it('uses PASCAL_LSP_BIN when the setting is unset', () => {
    const result = resolveServerBinary(undefined, makeEnv({
      env: { PASCAL_LSP_BIN: '~/bin/pascal-lsp' },
      isFile: (candidate) => candidate === '/home/u/bin/pascal-lsp',
    }));

    expect(result).toEqual({ ok: true, path: '/home/u/bin/pascal-lsp', source: 'env' });
  });

  it('reports a missing PASCAL_LSP_BIN target without falling through', () => {
    const checked: string[] = [];
    const result = resolveServerBinary(undefined, makeEnv({
      env: { PASCAL_LSP_BIN: '/custom/missing', PATH: '/bin' },
      isFile: (candidate) => {
        checked.push(candidate);
        return candidate === '/bin/pascal-lsp';
      },
    }));

    expect(result).toEqual({ ok: false, reason: 'missing-env', path: '/custom/missing' });
    expect(checked).toEqual(['/custom/missing']);
  });
});

describe('resolveServerBinary PATH search', () => {
  it('skips empty POSIX PATH segments', () => {
    const checked: string[] = [];
    const result = resolveServerBinary(undefined, makeEnv({
      env: { PATH: '/a::/b' },
      isFile: (candidate) => {
        checked.push(candidate);
        return candidate === '/b/pascal-lsp';
      },
    }));

    expect(result).toEqual({ ok: true, path: '/b/pascal-lsp', source: 'path' });
    expect(checked).toEqual(['/a/pascal-lsp', '/b/pascal-lsp']);
  });

  it('finds a Windows executable in a quoted PATH segment and reads Path case-insensitively', () => {
    const result = resolveServerBinary(undefined, makeEnv({
      platform: 'win32',
      env: { Path: 'C:\\a;"C:\\Program Files\\x";C:\\b' },
      isFile: (candidate) => candidate === 'C:\\Program Files\\x\\pascal-lsp.exe',
    }));

    expect(result).toEqual({
      ok: true,
      path: 'C:\\Program Files\\x\\pascal-lsp.exe',
      source: 'path',
    });
  });

  it('requires the .exe suffix on Windows', () => {
    const result = resolveServerBinary(undefined, makeEnv({
      platform: 'win32',
      env: { PATH: 'C:\\tools' },
      isFile: (candidate) => candidate === 'C:\\tools\\pascal-lsp',
    }));

    expect(result).toEqual({ ok: false, reason: 'not-found' });
  });

  it('skips a PATH candidate that is not a file', () => {
    const result = resolveServerBinary(undefined, makeEnv({
      env: { PATH: '/directory-named-pascal-lsp:/bin' },
      isFile: (candidate) => candidate === '/bin/pascal-lsp',
    }));

    expect(result).toEqual({ ok: true, path: '/bin/pascal-lsp', source: 'path' });
  });

  it('returns not-found when no configured source resolves', () => {
    expect(resolveServerBinary(undefined, makeEnv({ env: { PATH: '/a::/b' } })))
      .toEqual({ ok: false, reason: 'not-found' });
  });
});

describe('resolutionMessage', () => {
  it('includes build guidance and the setting name when the server is not found', () => {
    const message = resolutionMessage({ ok: false, reason: 'not-found' });

    expect(message).toContain(BUILD_HINT);
    expect(message).toContain('pascalLsp.serverPath');
  });

  it('names PASCAL_LSP_BIN and the missing path for an environment failure', () => {
    const message = resolutionMessage({ ok: false, reason: 'missing-env', path: '/custom/missing' });

    expect(message).toContain('PASCAL_LSP_BIN');
    expect(message).toContain('/custom/missing');
  });
});
