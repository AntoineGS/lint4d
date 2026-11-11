import { defineConfig } from '@vscode/test-cli';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
const repo = fileURLToPath(new URL('../../', import.meta.url));
const testConfigHome = fileURLToPath(new URL('.vscode-test/config/', import.meta.url));
const exe = process.platform === 'win32' ? '.exe' : '';
const candidates = [`${repo}target/release/pascal-lsp${exe}`, `${repo}target/debug/pascal-lsp${exe}`];
const bin = process.env.PASCAL_LSP_BIN || candidates.find((p) => existsSync(p));
if (!bin) throw new Error('Build pascal-lsp first: cargo build -p pascal-lsp');
export default defineConfig({
  files: 'out/test/integration/**/*.test.js',
  workspaceFolder: './test/integration/workspace',
  launchArgs: ['--disable-extensions', '--disable-workspace-trust'],
  env: { PASCAL_LSP_BIN: bin, XDG_CONFIG_HOME: testConfigHome },
  mocha: { timeout: 120000, ui: 'bdd' },
});
