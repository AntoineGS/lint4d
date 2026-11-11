import * as vscode from 'vscode';
import type { ClientHost } from './clientHost';
import {
  browseDestination,
  buildItems,
  buildSelectionMessage,
  Generation,
  installationItems,
  installationSelectionMessage,
  isCandidate,
  mainSourceConfirmed,
  projectMenuItems,
  projectMenuPlaceholder,
  sameBuildField,
  sameInstallationContext,
  sameProjectContext,
  SerialQueue,
  summarizeWarnings,
  type MenuItem,
} from './pickerLogic';
import {
  capabilityFlags,
  Methods,
  type BuildContext,
  type BuildField,
  type InstallationContext,
  type ProjectCatalogue,
  type ProjectContext,
} from './protocol';

interface ProjectMenuQuickPickItem extends vscode.QuickPickItem {
  action: MenuItem;
}

interface BrowseProjectQuickPickItem extends vscode.QuickPickItem {
  projectUri: string;
}

const didSelectEmitter = new vscode.EventEmitter<void>();
export const onDidSelect: vscode.Event<void> = didSelectEmitter.event;

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === 'object' && error !== null && 'message' in error) {
    return String(error.message);
  }
  return String(error);
}

function hasWarnings(value: unknown): value is { warnings: string[] } {
  return typeof value === 'object'
    && value !== null
    && 'warnings' in value
    && Array.isArray(value.warnings);
}

export function registerPickers(host: ClientHost): vscode.Disposable {
  const generation = new Generation();
  const queue = new SerialQueue();
  const onClientStart = host.onDidStart(() => generation.next());

  const reportWarnings = (warnings: readonly string[]): void => {
    for (const warning of warnings) host.output.warn(warning);
    const message = summarizeWarnings(warnings);
    if (message) {
      void vscode.window.showWarningMessage(message, 'Show Output').then((choice) => {
        if (choice === 'Show Output') host.output.show();
      });
    }
  };

  const activePascalUri = (): string | undefined => {
    const document = vscode.window.activeTextEditor?.document;
    if (!document || document.uri.scheme !== 'file' || document.languageId !== 'pascal') {
      void vscode.window.showWarningMessage('Open a Pascal file first.');
      return undefined;
    }
    const client = host.client;
    if (!client) {
      void vscode.window.showWarningMessage('Pascal LSP is not running.');
      return undefined;
    }
    return client.code2ProtocolConverter.asUri(document.uri);
  };

  const requireFlag = (flag: keyof ReturnType<typeof capabilityFlags>, feature: string): boolean => {
    if (capabilityFlags(host.experimental())[flag]) return true;
    void vscode.window.showWarningMessage(
      `Installed Pascal LSP does not support ${feature}; update the server.`,
    );
    return false;
  };

  const request = async <T>(method: string, params: unknown, gen: number): Promise<T | undefined> => {
    if (!generation.isCurrent(gen)) return undefined;
    const client = host.client;
    if (!client) {
      void vscode.window.showWarningMessage('Pascal LSP is not running.');
      return undefined;
    }
    try {
      const response = await client.sendRequest<T>(method, params);
      if (!generation.isCurrent(gen) || host.client !== client) return undefined;
      if (hasWarnings(response)) reportWarnings(response.warnings);
      return response;
    } catch (error) {
      if (!generation.isCurrent(gen) || host.client !== client) return undefined;
      void vscode.window.showErrorMessage(errorMessage(error));
      return undefined;
    }
  };

  const pick = <T extends vscode.QuickPickItem>(
    items: readonly T[],
    placeholder: string,
    title?: string,
  ): Thenable<T | undefined> => vscode.window.showQuickPick(items, {
    placeHolder: placeholder,
    ...(title === undefined ? {} : { title }),
    ignoreFocusOut: false,
  });

  const openDocument = async (uri: string, gen: number): Promise<void> => {
    if (!generation.isCurrent(gen)) return;
    const client = host.client;
    if (!client) return;
    await vscode.window.showTextDocument(client.protocol2CodeConverter.asUri(uri));
  };

  const selectProject = async (
    uri: string,
    context: ProjectContext,
    projectUri: string | null,
    gen: number,
  ): Promise<void> => {
    await queue.run(async () => {
      const fresh = await request<ProjectContext>(
        Methods.projectContext,
        { textDocument: { uri } },
        gen,
      );
      if (!fresh) return;
      if (!sameProjectContext(fresh, context)) {
        void vscode.window.showWarningMessage('Choices changed while the picker was open; reopen it.');
        return;
      }

      const selected = await request<ProjectContext>(
        Methods.selectProject,
        { textDocument: { uri }, projectUri },
        gen,
      );
      if (!selected) return;
      void vscode.window.showInformationMessage(`Pascal project selection: ${selected.selectionMode}`);
      didSelectEmitter.fire();
    });
  };

  const browseProjects = async (
    uri: string,
    previousContext: ProjectContext | undefined,
    gen: number,
  ): Promise<void> => {
    if (!requireFlag('projectCatalogue', 'repository project browsing')) return;
    const context = previousContext ?? await request<ProjectContext>(
      Methods.projectContext,
      { textDocument: { uri } },
      gen,
    );
    if (!context) return;

    const catalogue = await request<ProjectCatalogue>(
      Methods.listProjects,
      { textDocument: { uri } },
      gen,
    );
    if (!catalogue) return;
    if (catalogue.projects.length === 0) {
      void vscode.window.showInformationMessage('No Delphi projects found in this repository');
      return;
    }

    const items: BrowseProjectQuickPickItem[] = catalogue.projects.map((project) => ({
      label: project.label,
      ...(project.projectUri === context.selectedProjectUri ? { description: 'current' } : {}),
      projectUri: project.projectUri,
    }));
    const choice = await pick(
      items,
      '',
      catalogue.complete ? 'Browse Delphi projects' : 'Browse Delphi projects (incomplete catalogue)',
    );
    if (!choice || !generation.isCurrent(gen)) return;

    const fresh = await request<ProjectContext>(
      Methods.projectContext,
      { textDocument: { uri } },
      gen,
    );
    if (!fresh) return;
    if (!sameProjectContext(fresh, context)) {
      void vscode.window.showWarningMessage('Choices changed while the picker was open; reopen it.');
      return;
    }

    await queue.run(async () => {
      const targetUri = choice.projectUri;
      const target = await request<ProjectContext>(
        Methods.projectContext,
        { textDocument: { uri: targetUri } },
        gen,
      );
      if (!target) return;
      if (!isCandidate(target, targetUri)) {
        void vscode.window.showWarningMessage('Browsed project is no longer a candidate; reopen the picker.');
        return;
      }

      const selected = await request<ProjectContext>(
        Methods.selectProject,
        { textDocument: { uri: targetUri }, projectUri: targetUri },
        gen,
      );
      if (!selected) return;
      didSelectEmitter.fire();

      const destination = browseDestination(context, uri, targetUri, selected);
      if (destination.kind === 'stay' || !generation.isCurrent(gen)) return;
      if (destination.kind === 'open-project') {
        void vscode.window.showWarningMessage(destination.reason);
        await openDocument(targetUri, gen);
        return;
      }

      const source = await request<ProjectContext>(
        Methods.projectContext,
        { textDocument: { uri: destination.mainSourceUri } },
        gen,
      );
      if (!generation.isCurrent(gen)) return;
      if (source && mainSourceConfirmed(source, targetUri)) {
        await openDocument(destination.mainSourceUri, gen);
      } else {
        void vscode.window.showWarningMessage(
          'Main source does not resolve to the browsed project; opening the project file instead.',
        );
        await openDocument(targetUri, gen);
      }
    });
  };

  const selectBuildField = async (uri: string, field: BuildField, gen: number): Promise<void> => {
    const feature = field === 'config' ? 'build configuration selection' : 'platform selection';
    if (!requireFlag('buildSelection', feature)) return;

    const projectContext = await request<ProjectContext>(
      Methods.projectContext,
      { textDocument: { uri } },
      gen,
    );
    if (!projectContext) return;
    const projectUri = projectContext.selectedProjectUri;
    if (projectUri === null) {
      void vscode.window.showWarningMessage('Project selection is unresolved; choose a project first.');
      return;
    }

    const build = await request<BuildContext>(Methods.buildContext, { projectUri }, gen);
    if (!build) return;
    const choice = await pick(
      buildItems(build[field]),
      `${field === 'config' ? 'Build configuration' : 'Platform'} (${build[field].mode})`,
    );
    if (!choice || !generation.isCurrent(gen)) return;

    await queue.run(async () => {
      const fresh = await request<BuildContext>(Methods.buildContext, { projectUri }, gen);
      if (!fresh) return;
      if (!sameBuildField(build[field], fresh[field])) {
        void vscode.window.showWarningMessage('Choices changed while the picker was open; reopen it.');
        return;
      }

      const method = field === 'config' ? Methods.selectBuildConfig : Methods.selectPlatform;
      const params = field === 'config'
        ? { projectUri, config: choice.value }
        : { projectUri, platform: choice.value };
      const selected = await request<BuildContext>(method, params, gen);
      if (!selected) return;
      void vscode.window.showInformationMessage(buildSelectionMessage(field, selected));
      didSelectEmitter.fire();
    });
  };

  const selectInstallation = async (uri: string, gen: number): Promise<void> => {
    if (!requireFlag('installationSelection', 'Delphi installation selection')) return;
    const projectContext = await request<ProjectContext>(
      Methods.projectContext,
      { textDocument: { uri } },
      gen,
    );
    if (!projectContext) return;
    const projectUri = projectContext.selectedProjectUri;
    if (projectUri === null) {
      void vscode.window.showWarningMessage('Project selection is unresolved; choose a project first.');
      return;
    }

    const installation = await request<InstallationContext>(
      Methods.installationContext,
      { projectUri },
      gen,
    );
    if (!installation) return;
    const choice = await pick(
      installationItems(installation),
      `Delphi installation (${installation.selectionMode})`,
    );
    if (!choice || !generation.isCurrent(gen)) return;

    await queue.run(async () => {
      const freshProject = await request<ProjectContext>(
        Methods.projectContext,
        { textDocument: { uri } },
        gen,
      );
      if (!freshProject) return;
      if (!sameProjectContext(freshProject, projectContext)) {
        void vscode.window.showWarningMessage('Choices changed while the picker was open; reopen it.');
        return;
      }

      const freshInstallation = await request<InstallationContext>(
        Methods.installationContext,
        { projectUri },
        gen,
      );
      if (!freshInstallation) return;
      if (!sameInstallationContext(freshInstallation, installation)) {
        void vscode.window.showWarningMessage('Choices changed while the picker was open; reopen it.');
        return;
      }

      const selected = await request<InstallationContext>(
        Methods.selectInstallation,
        { projectUri, installationId: choice.value },
        gen,
      );
      if (!selected) return;
      void vscode.window.showInformationMessage(installationSelectionMessage(selected));
      didSelectEmitter.fire();
    });
  };

  const projectMenu = async (): Promise<void> => {
    const gen = generation.next();
    const uri = activePascalUri();
    if (!uri || !requireFlag('projectSelection', 'project selection')) return;
    const context = await request<ProjectContext>(
      Methods.projectContext,
      { textDocument: { uri } },
      gen,
    );
    if (!context) return;

    const menuItems = projectMenuItems(context, capabilityFlags(host.experimental()));
    const quickPickItems: (ProjectMenuQuickPickItem | vscode.QuickPickItem)[] = [];
    let insertedSeparator = false;
    for (const item of menuItems) {
      if (item.kind !== 'project' && !insertedSeparator) {
        quickPickItems.push({ label: '', kind: vscode.QuickPickItemKind.Separator });
        insertedSeparator = true;
      }
      quickPickItems.push({
        label: item.label,
        ...('description' in item && item.description !== undefined
          ? { description: item.description }
          : {}),
        action: item,
      });
    }

    const choice = await pick<ProjectMenuQuickPickItem | vscode.QuickPickItem>(
      quickPickItems,
      projectMenuPlaceholder(context),
    );
    if (!choice || !generation.isCurrent(gen) || !('action' in choice)) return;
    const action = choice.action;
    switch (action.kind) {
      case 'project':
        await selectProject(uri, context, action.projectUri, gen);
        break;
      case 'browse':
        await browseProjects(uri, context, gen);
        break;
      case 'installation':
        await selectInstallation(uri, gen);
        break;
      case 'build-config':
        await selectBuildField(uri, 'config', gen);
        break;
      case 'platform':
        await selectBuildField(uri, 'platform', gen);
        break;
    }
  };

  const runForActiveDocument = async (
    operation: (uri: string, gen: number) => Promise<void>,
  ): Promise<void> => {
    const gen = generation.next();
    const uri = activePascalUri();
    if (!uri) return;
    await operation(uri, gen);
  };

  return vscode.Disposable.from(
    onClientStart,
    vscode.commands.registerCommand('pascal.projectMenu', projectMenu),
    vscode.commands.registerCommand('pascal.browseProjects', () => (
      runForActiveDocument((uri, gen) => browseProjects(uri, undefined, gen))
    )),
    vscode.commands.registerCommand('pascal.selectBuildConfig', () => (
      runForActiveDocument((uri, gen) => selectBuildField(uri, 'config', gen))
    )),
    vscode.commands.registerCommand('pascal.selectPlatform', () => (
      runForActiveDocument((uri, gen) => selectBuildField(uri, 'platform', gen))
    )),
    vscode.commands.registerCommand('pascal.selectInstallation', () => (
      runForActiveDocument(selectInstallation)
    )),
  );
}
