import * as path from 'node:path';

export const BUILD_HINT = 'cargo build --release --locked -p pascal-lsp';

export interface ResolveEnv {
  platform: NodeJS.Platform;
  env: Record<string, string | undefined>;
  homedir: string;
  workspaceFolder: string | undefined;
  isFile: (path: string) => boolean;
}

export type Resolution =
  | { ok: true; path: string; source: 'setting' | 'env' | 'path' }
  | { ok: false; reason: 'missing-setting' | 'missing-env'; path: string }
  | { ok: false; reason: 'not-found' };

function lib(e: ResolveEnv): typeof path.posix | typeof path.win32 {
  return e.platform === 'win32' ? path.win32 : path.posix;
}

export function expandPath(input: string, e: ResolveEnv): string {
  let out = input;
  if (e.workspaceFolder !== undefined) out = out.split('${workspaceFolder}').join(e.workspaceFolder);
  if (out === '~' || out.startsWith('~/') || out.startsWith('~\\')) out = lib(e).join(e.homedir, out.slice(1));
  return out;
}

function envValue(e: ResolveEnv, name: string): string | undefined {
  if (e.platform !== 'win32') return e.env[name];
  const key = Object.keys(e.env).find((candidate) => candidate.toUpperCase() === name.toUpperCase());
  return key === undefined ? undefined : e.env[key];
}

export function resolveServerBinary(serverPath: string | undefined, e: ResolveEnv): Resolution {
  if (serverPath !== undefined && serverPath.trim() !== '') {
    const resolvedPath = expandPath(serverPath.trim(), e);
    return e.isFile(resolvedPath)
      ? { ok: true, path: resolvedPath, source: 'setting' }
      : { ok: false, reason: 'missing-setting', path: resolvedPath };
  }

  const fromEnv = envValue(e, 'PASCAL_LSP_BIN');
  if (fromEnv !== undefined && fromEnv.trim() !== '') {
    const resolvedPath = expandPath(fromEnv.trim(), e);
    return e.isFile(resolvedPath)
      ? { ok: true, path: resolvedPath, source: 'env' }
      : { ok: false, reason: 'missing-env', path: resolvedPath };
  }

  const name = e.platform === 'win32' ? 'pascal-lsp.exe' : 'pascal-lsp';
  const separator = e.platform === 'win32' ? ';' : ':';
  for (const raw of (envValue(e, 'PATH') ?? '').split(separator)) {
    const directory = raw.trim().replace(/^"(.*)"$/, '$1');
    if (directory === '') continue;
    const candidate = lib(e).join(directory, name);
    if (e.isFile(candidate)) return { ok: true, path: candidate, source: 'path' };
  }
  return { ok: false, reason: 'not-found' };
}

export function resolutionMessage(r: Exclude<Resolution, { ok: true }>): string {
  switch (r.reason) {
    case 'missing-setting': return `pascalLsp.serverPath points to a missing file: ${r.path}`;
    case 'missing-env': return `PASCAL_LSP_BIN points to a missing file: ${r.path}`;
    case 'not-found': return `pascal-lsp was not found on PATH. Build it with \`${BUILD_HINT}\` and set pascalLsp.serverPath, or put it on PATH.`;
  }
}
