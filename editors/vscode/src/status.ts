import * as vscode from 'vscode';
import type { ClientHost } from './clientHost';

export function registerStatus(host: ClientHost): vscode.Disposable {
  void host;
  return vscode.Disposable.from();
}
