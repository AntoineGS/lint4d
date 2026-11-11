import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { ExtensionApi } from '../../src/extension';

async function until<T>(probe: () => T | undefined | Promise<T | undefined>, what: string, ms = 90000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = await probe();
    if (v !== undefined) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 250));
  }
}

describe('pascal-lsp extension', () => {
  const root = vscode.workspace.workspaceFolders![0]!.uri;
  const main = vscode.Uri.joinPath(root, 'src', 'Main.pas');
  let api: ExtensionApi;

  before(async () => {
    const doc = await vscode.workspace.openTextDocument(main);
    await vscode.window.showTextDocument(doc);
    const ext = vscode.extensions.getExtension<ExtensionApi>('multidev.pascal-lsp')!;
    api = await ext.activate();
    await until(() => (api.host.status === 'running' ? true : undefined), 'client running');
  });

  it('publishes the with-statement diagnostic', async () => {
    const d = await until(() => vscode.languages.getDiagnostics(main).find((x) => String(x.code ?? '').includes('with-statement') || x.message.includes("'with'")), 'with diagnostic');
    assert.equal(d.range.start.line, 20);
  });

  it('goes to the definition in another unit', async () => {
    const locations = await until(async () => {
      const r = await vscode.commands.executeCommand<(vscode.Location | vscode.LocationLink)[]>('vscode.executeDefinitionProvider', main, new vscode.Position(21, 10));
      return r && r.length > 0 ? r : undefined;
    }, 'definition');
    const target = 'targetUri' in locations[0]! ? locations[0]!.targetUri : locations[0]!.uri;
    assert.ok(target.path.endsWith('/src/Helpers.pas'), target.toString());
  });

  it('renders the status document', async () => {
    await vscode.commands.executeCommand('pascal.showStatus');
    const doc = await until(() => vscode.window.activeTextEditor?.document.uri.scheme === 'pascal-lsp-status' ? vscode.window.activeTextEditor.document : undefined, 'status document');
    assert.match(doc.getText(), /"indexStatus"/);
    assert.match(doc.getText(), /"cacheStatus"/);
  });

  it('restarts', async () => {
    const before = api.host.startCount;
    await vscode.commands.executeCommand('pascal.restartServer');
    await until(() => (api.host.status === 'running' && api.host.startCount > before ? true : undefined), 'restarted client');
  });
});
