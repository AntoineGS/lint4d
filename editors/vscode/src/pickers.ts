import * as vscode from 'vscode';
import type { ClientHost } from './clientHost';

export function registerPickers(host: ClientHost): vscode.Disposable {
  void host;
  return vscode.Disposable.from();
}
