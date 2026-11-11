import * as vscode from 'vscode';
import type { ClientHost } from './clientHost';
import {
  capabilityFlags,
  Methods,
  type BuildContext,
  type CacheStatus,
  type IndexStatus,
  type InstallationContext,
  type ProjectContext,
} from './protocol';
import { onDidSelect } from './pickers';
import {
  FAST_POLL_MS,
  pollInterval,
  shouldPollStatus,
  statusText,
  statusTooltip,
  type StatusSnapshot,
} from './statusFormat';

const STATUS_DOCUMENT_URI = vscode.Uri.parse('pascal-lsp-status:/Pascal%20LSP%20Status.json');
const STATUS_SCHEME = 'pascal-lsp-status';
const CONTEXT_DEBOUNCE_MS = 300;

interface FetchedStatus {
  projectContext: ProjectContext | null;
  buildContext: BuildContext | null;
  installationContext: InstallationContext | null;
  indexStatus: IndexStatus | null;
  cacheStatus: CacheStatus | null;
  errors: Record<string, string>;
}

function emptyFetchedStatus(): FetchedStatus {
  return {
    projectContext: null,
    buildContext: null,
    installationContext: null,
    indexStatus: null,
    cacheStatus: null,
    errors: {},
  };
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === 'object' && error !== null && 'message' in error) {
    return String(error.message);
  }
  return String(error);
}

export function registerStatus(host: ClientHost): vscode.Disposable {
  const item = vscode.window.createStatusBarItem('pascalLsp.status', vscode.StatusBarAlignment.Left, 100);
  item.name = 'Pascal LSP';
  item.command = 'pascal.projectMenu';

  const changeEmitter = new vscode.EventEmitter<vscode.Uri>();
  let projectContext: ProjectContext | undefined;
  let buildContext: BuildContext | undefined;
  let installationContext: InstallationContext | undefined;
  let indexStatus: IndexStatus | undefined;
  let cacheStatus: CacheStatus | undefined;
  let stale = false;
  let disposed = false;
  let inFlight = false;
  let requestTail: Promise<void> = Promise.resolve();
  let refreshWorker: Promise<void> | undefined;
  let contextRefreshPending = false;
  let serverRefreshPending = false;
  let contextDebounce: ReturnType<typeof setTimeout> | undefined;
  let pollTimer: ReturnType<typeof setTimeout> | undefined;
  let forceFastPoll = false;
  let lastPascalFile: vscode.Uri | undefined;

  const currentSnapshot = (): StatusSnapshot => ({
    clientStatus: host.status,
    ...(projectContext ? { project: projectContext } : {}),
    ...(buildContext ? { build: buildContext } : {}),
    ...(installationContext ? { installation: installationContext } : {}),
    ...(indexStatus ? { index: indexStatus } : {}),
    ...(cacheStatus ? { cache: cacheStatus } : {}),
    stale,
  });

  const render = (): void => {
    if (disposed) return;
    const snapshot = currentSnapshot();
    item.text = statusText(snapshot);
    const tooltip = new vscode.MarkdownString(statusTooltip(snapshot));
    tooltip.supportThemeIcons = true;
    item.tooltip = tooltip;

    const enabled = vscode.workspace.getConfiguration('pascalLsp').get<boolean>('statusBar.enabled', true);
    if (enabled && vscode.window.activeTextEditor?.document.languageId === 'pascal') item.show();
    else item.hide();
  };

  const hasVisiblePascalEditor = (): boolean => (
    vscode.window.visibleTextEditors.some((editor) => editor.document.languageId === 'pascal')
  );

  const canPoll = (): boolean => shouldPollStatus({
    focused: vscode.window.state.focused,
    clientStatus: host.status,
    hasVisiblePascalEditor: hasVisiblePascalEditor(),
  });

  const clearPollTimer = (): void => {
    if (pollTimer !== undefined) clearTimeout(pollTimer);
    pollTimer = undefined;
  };

  const withRequestLock = <T>(operation: () => Promise<T>): Promise<T> => {
    const previous = requestTail;
    let release!: () => void;
    requestTail = new Promise<void>((resolve) => { release = resolve; });
    return previous.then(async () => {
      try {
        return await operation();
      } finally {
        release();
      }
    });
  };

  const isCurrentClient = (
    client: NonNullable<ClientHost['client']>,
    startCount: number,
  ): boolean => !disposed && host.client === client && host.startCount === startCount;

  const fetchStatus = async (
    client: NonNullable<ClientHost['client']>,
    startCount: number,
    documentUri: vscode.Uri | undefined,
    fetchContext: boolean,
    fetchServer: boolean,
  ): Promise<FetchedStatus> => {
    const result = emptyFetchedStatus();
    const request = async <T>(name: keyof Omit<FetchedStatus, 'errors'>, method: string, params?: unknown): Promise<T | null> => {
      if (!isCurrentClient(client, startCount)) return null;
      try {
        const value = params === undefined
          ? await client.sendRequest<T>(method)
          : await client.sendRequest<T>(method, params);
        return isCurrentClient(client, startCount) ? value : null;
      } catch (error) {
        if (isCurrentClient(client, startCount)) result.errors[name] = errorMessage(error);
        return null;
      }
    };

    const fetchContexts = async (): Promise<void> => {
      if (!documentUri || documentUri.scheme !== 'file') return;
      const uri = client.code2ProtocolConverter.asUri(documentUri);
      result.projectContext = await request<ProjectContext>(
        'projectContext',
        Methods.projectContext,
        { textDocument: { uri } },
      );
      const projectUri = result.projectContext?.selectedProjectUri;
      if (typeof projectUri !== 'string') return;

      const tasks: Promise<void>[] = [
        request<BuildContext>('buildContext', Methods.buildContext, { projectUri })
          .then((value) => { result.buildContext = value; }),
      ];
      if (capabilityFlags(host.experimental()).installationSelection) {
        tasks.push(request<InstallationContext>(
          'installationContext',
          Methods.installationContext,
          { projectUri },
        ).then((value) => { result.installationContext = value; }));
      }
      await Promise.all(tasks);
    };

    const fetchServerStatus = async (): Promise<void> => {
      const [index, cache] = await Promise.all([
        request<IndexStatus>('indexStatus', Methods.indexStatus),
        request<CacheStatus>('cacheStatus', Methods.cacheStatus),
      ]);
      result.indexStatus = index;
      result.cacheStatus = cache;
    };

    await Promise.all([
      fetchContext ? fetchContexts() : Promise.resolve(),
      fetchServer ? fetchServerStatus() : Promise.resolve(),
    ]);
    return result;
  };

  const applyRefresh = async (fetchContext: boolean, fetchServer: boolean): Promise<void> => {
    const client = host.client;
    if (!client) return;
    const startCount = host.startCount;
    const document = vscode.window.activeTextEditor?.document;
    const documentUri = document?.uri.scheme === 'file' && document.languageId === 'pascal'
      ? document.uri
      : undefined;

    const result = await fetchStatus(client, startCount, documentUri, fetchContext, fetchServer);
    if (!isCurrentClient(client, startCount)) return;

    const contextFailed = Object.keys(result.errors).some((key) => (
      key === 'projectContext' || key === 'buildContext' || key === 'installationContext'
    ));
    const serverFailed = Object.keys(result.errors).some((key) => key === 'indexStatus' || key === 'cacheStatus');
    if (fetchContext && !contextFailed) {
      projectContext = result.projectContext ?? undefined;
      buildContext = result.buildContext ?? undefined;
      installationContext = result.installationContext ?? undefined;
    }
    if (fetchServer && !serverFailed) {
      indexStatus = result.indexStatus ?? undefined;
      cacheStatus = result.cacheStatus ?? undefined;
    }
    stale = Object.keys(result.errors).length > 0;
    render();
  };

  const refresh = (fetchContext: boolean, fetchServer: boolean): Promise<void> => {
    if (disposed) return Promise.resolve();
    contextRefreshPending ||= fetchContext;
    serverRefreshPending ||= fetchServer;
    if (inFlight && refreshWorker) return refreshWorker;
    if (refreshWorker) return refreshWorker;

    const drain = async (): Promise<void> => {
      while (!disposed && (contextRefreshPending || serverRefreshPending)) {
        const wantsContext = contextRefreshPending;
        const wantsServer = serverRefreshPending;
        contextRefreshPending = false;
        serverRefreshPending = false;
        inFlight = true;
        try {
          await withRequestLock(async () => {
            try {
              await applyRefresh(wantsContext, wantsServer);
            } catch (error) {
              stale = true;
              host.output.error(`Failed to refresh Pascal LSP status: ${errorMessage(error)}`);
              render();
            }
          });
        } finally {
          inFlight = false;
        }
      }
    };

    refreshWorker = drain().finally(() => {
      refreshWorker = undefined;
      if (contextRefreshPending || serverRefreshPending) void refresh(false, false);
    });
    return refreshWorker;
  };

  const scheduleNextPoll = (): void => {
    if (!canPoll() || pollTimer !== undefined || disposed) return;
    const delay = forceFastPoll ? FAST_POLL_MS : pollInterval(indexStatus);
    forceFastPoll = false;
    pollTimer = setTimeout(() => {
      pollTimer = undefined;
      void refreshServerStatus();
    }, delay);
  };

  const refreshServerStatus = async (): Promise<void> => {
    if (!canPoll() || disposed) {
      clearPollTimer();
      return;
    }
    await refresh(false, true);
    if (canPoll() && !disposed) scheduleNextPoll();
  };

  const updatePollLoop = (immediate = false): void => {
    if (!canPoll() || disposed) {
      clearPollTimer();
      return;
    }
    if (immediate) {
      clearPollTimer();
      void refreshServerStatus();
      return;
    }
    scheduleNextPoll();
  };

  const refreshAndSchedule = (fetchContext: boolean, fetchServer: boolean): void => {
    clearPollTimer();
    void refresh(fetchContext, fetchServer).then(() => scheduleNextPoll());
  };

  const rememberActivePascalFile = (editor: vscode.TextEditor | undefined): void => {
    if (editor?.document.uri.scheme === STATUS_SCHEME) return;
    if (editor?.document.uri.scheme === 'file' && editor.document.languageId === 'pascal') {
      lastPascalFile = editor.document.uri;
    } else {
      lastPascalFile = undefined;
    }
  };

  const provideStatusDocument = async (): Promise<string> => withRequestLock(async () => {
    const client = host.client;
    const startCount = host.startCount;
    let result = emptyFetchedStatus();
    if (client) {
      try {
        result = await fetchStatus(client, startCount, lastPascalFile, true, true);
      } catch (error) {
        result.errors.status = errorMessage(error);
      }
      if (!isCurrentClient(client, startCount)) result = emptyFetchedStatus();
    }

    const currentClient = host.client;
    return JSON.stringify({
      server: currentClient?.initializeResult?.serverInfo ?? null,
      binary: host.binaryPath ?? null,
      clientStatus: host.status,
      projectContext: result.projectContext,
      buildContext: result.buildContext,
      installationContext: result.installationContext,
      indexStatus: result.indexStatus,
      cacheStatus: result.cacheStatus,
      errors: result.errors,
    }, null, 2);
  });

  const provider: vscode.TextDocumentContentProvider = {
    onDidChange: changeEmitter.event,
    provideTextDocumentContent: provideStatusDocument,
  };

  const showStatus = async (): Promise<void> => {
    rememberActivePascalFile(vscode.window.activeTextEditor);
    const document = await vscode.workspace.openTextDocument(STATUS_DOCUMENT_URI);
    const jsonDocument = await vscode.languages.setTextDocumentLanguage(document, 'json');
    await vscode.window.showTextDocument(jsonDocument);
  };

  const refreshStatusDocument = (): void => {
    changeEmitter.fire(STATUS_DOCUMENT_URI);
  };

  const scheduleContextRefresh = (): void => {
    if (contextDebounce !== undefined) clearTimeout(contextDebounce);
    contextDebounce = setTimeout(() => {
      contextDebounce = undefined;
      void refresh(true, false);
    }, CONTEXT_DEBOUNCE_MS);
  };

  rememberActivePascalFile(vscode.window.activeTextEditor);
  render();

  const subscriptions: vscode.Disposable[] = [
    item,
    changeEmitter,
    vscode.workspace.registerTextDocumentContentProvider(STATUS_SCHEME, provider),
    vscode.commands.registerCommand('pascal.showStatus', showStatus),
    vscode.commands.registerCommand('pascal.refreshStatus', refreshStatusDocument),
    onDidSelect(() => {
      forceFastPoll = true;
      refreshAndSchedule(true, canPoll());
    }),
    host.onDidStart(() => {
      refreshAndSchedule(true, canPoll());
    }),
    host.onDidChangeStatus(() => {
      render();
      updatePollLoop();
    }),
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      rememberActivePascalFile(editor);
      render();
      scheduleContextRefresh();
      updatePollLoop();
    }),
    vscode.window.onDidChangeVisibleTextEditors(() => updatePollLoop(true)),
    vscode.window.onDidChangeWindowState((state) => updatePollLoop(state.focused)),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('pascalLsp.statusBar.enabled')) render();
    }),
  ];

  return vscode.Disposable.from(...subscriptions, {
    dispose: () => {
      disposed = true;
      if (contextDebounce !== undefined) clearTimeout(contextDebounce);
      clearPollTimer();
    },
  });
}
