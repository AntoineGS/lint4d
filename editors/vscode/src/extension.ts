import * as vscode from 'vscode';
export function activate(context: vscode.ExtensionContext): void {
  context.subscriptions.push(vscode.window.createOutputChannel('Pascal LSP', { log: true }));
}
export function deactivate(): void {}
