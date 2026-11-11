import * as vscode from 'vscode';
import { createClientHost, type ClientHost } from './clientHost';
import { registerDcu } from './dcu';
import { registerPickers } from './pickers';
import { registerStatus } from './status';

export interface ExtensionApi {
  readonly host: ClientHost;
}

let host: ClientHost | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<ExtensionApi> {
  host = createClientHost(context);
  context.subscriptions.push(
    host,
    vscode.commands.registerCommand('pascal.restartServer', () => host?.restart()),
    vscode.commands.registerCommand('pascal.showOutput', () => host?.output.show()),
    registerDcu(host),
    registerPickers(host),
    registerStatus(host),
  );
  void host.start();
  return { host };
}

export async function deactivate(): Promise<void> {
  await host?.stop();
}
