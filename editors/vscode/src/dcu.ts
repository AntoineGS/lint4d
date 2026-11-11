import * as vscode from 'vscode';
import type { ClientHost } from './clientHost';
import type { DocumentSymbol, SymbolInformation } from 'vscode-languageclient/node';
import { dcuCapability, dcuErrorText } from './dcuText';

export function registerDcu(host: ClientHost): vscode.Disposable {
  let registration: vscode.Disposable | undefined;

  const disposeRegistration = (): void => {
    registration?.dispose();
    registration = undefined;
  };

  const onDidStart = host.onDidStart(() => {
    disposeRegistration();
    const capability = dcuCapability(host.experimental());
    if (!capability) return;

    const contentProvider = vscode.workspace.registerTextDocumentContentProvider(
      capability.uriScheme,
      {
        provideTextDocumentContent: async (uri, token) => {
          const client = host.client;
          if (!client) return dcuErrorText(new Error('Pascal LSP is not running'));
          try {
            const result = await client.sendRequest<{ text: string }>(
              capability.contentMethod,
              { textDocument: { uri: client.code2ProtocolConverter.asUri(uri) } },
              token,
            );
            return result.text;
          } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            host.output.warn(`compiled view ${uri.toString()}: ${message}`);
            return dcuErrorText(error);
          }
        },
      },
    );

    const symbolProvider = vscode.languages.registerDocumentSymbolProvider(
      { scheme: capability.uriScheme },
      {
        provideDocumentSymbols: async (document, token) => {
          const client = host.client;
          if (!client) return [];
          try {
            const result = await client.sendRequest<DocumentSymbol[] | SymbolInformation[]>(
              'textDocument/documentSymbol',
              { textDocument: { uri: client.code2ProtocolConverter.asUri(document.uri) } },
              token,
            );
            if (!Array.isArray(result) || result.length === 0) return [];
            const first = result[0];
            if (!first) return [];
            return 'selectionRange' in first
              ? client.protocol2CodeConverter.asDocumentSymbols(result as DocumentSymbol[], token)
              : client.protocol2CodeConverter.asSymbolInformations(result as SymbolInformation[], token);
          } catch {
            return [];
          }
        },
      },
    );

    registration = vscode.Disposable.from(contentProvider, symbolProvider);
  });

  const onDidChangeStatus = host.onDidChangeStatus((status) => {
    if (status === 'stopped' || status === 'failed') disposeRegistration();
  });

  return vscode.Disposable.from(
    onDidStart,
    onDidChangeStatus,
    new vscode.Disposable(disposeRegistration),
  );
}
