import * as fs from 'node:fs';
import * as os from 'node:os';
import * as vscode from 'vscode';
import {
  CloseAction,
  DidChangeConfigurationNotification,
  ErrorAction,
  LanguageClient,
  State,
  TransportKind,
  type CloseHandlerResult,
  type LanguageClientOptions,
  type ServerOptions,
} from 'vscode-languageclient/node';
import {
  affectsServerSettings,
  buildInitializationOptions,
  buildServerSettings,
  requiresRestart,
  type ConfigReader,
} from './settings';
import { resolutionMessage, resolveServerBinary } from './server';
import { capabilityFlags, EXPERIMENTAL_FLAGS, type ExperimentalFlag } from './protocol';
import {
  announceClientStarted,
  CrashWindow,
  SerialQueue,
  startOutcome,
  shouldRetryClosedClient,
} from './clientHostLogic';

export type ClientStatus = 'stopped' | 'starting' | 'running' | 'failed';

export interface ClientHost extends vscode.Disposable {
  readonly output: vscode.LogOutputChannel;
  readonly status: ClientStatus;
  readonly client: LanguageClient | undefined;
  readonly binaryPath: string | undefined;
  readonly startCount: number;
  readonly onDidStart: vscode.Event<LanguageClient>;
  readonly onDidChangeStatus: vscode.Event<ClientStatus>;
  experimental(): Record<string, unknown>;
  start(): Promise<void>;
  restart(): Promise<void>;
  stop(): Promise<void>;
}

export const CAPABILITY_CONTEXT_KEYS: Record<string, string> = Object.fromEntries(
  EXPERIMENTAL_FLAGS.map((flag) => [flag, `pascalLsp.${flag}`]),
);

const STOP_TIMEOUT_MS = 6000;

export function configReader(): ConfigReader {
  const configuration = vscode.workspace.getConfiguration('pascalLsp');
  return (key) => {
    const inspected = configuration.inspect(key);
    return inspected?.workspaceValue ?? inspected?.globalValue;
  };
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function stringArray(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === 'string') : [];
}

function stringEnvironment(value: unknown): Record<string, string> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return {};
  return Object.fromEntries(
    Object.entries(value).filter((entry): entry is [string, string] => typeof entry[1] === 'string'),
  );
}

class ClientHostImpl implements ClientHost {
  readonly output: vscode.LogOutputChannel;
  private readonly didStartEmitter = new vscode.EventEmitter<LanguageClient>();
  private readonly didChangeStatusEmitter = new vscode.EventEmitter<ClientStatus>();
  private activeClient: LanguageClient | undefined;
  private stateListener: vscode.Disposable | undefined;
  private configListener: vscode.Disposable | undefined;
  private currentStatus: ClientStatus = 'stopped';
  private currentBinaryPath: string | undefined;
  private successfulStarts = 0;
  private activeRunStarted = false;
  private activeRunAnnounced = false;
  private activeRunGeneration = 0;
  private activeClientReady = false;
  private closedBeforeStartResolved = false;
  private stopRequested = false;
  private disposed = false;
  private desiredRunning = false;
  private readonly crashWindow = new CrashWindow();
  private readonly lifecycleQueue = new SerialQueue();
  private readonly contextQueue = new SerialQueue();

  constructor(context: vscode.ExtensionContext) {
    this.output = vscode.window.createOutputChannel('Pascal LSP', { log: true });
    this.configListener = vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (this.disposed) return;

      const affects = (key: string) => event.affectsConfiguration(key);
      try {
        if (requiresRestart(affects)) {
          const choice = await vscode.window.showInformationMessage(
            'Restart Pascal LSP to apply the changed settings?',
            'Restart now',
          );
          if (choice === 'Restart now') await this.restart();
          return;
        }

        if (affectsServerSettings(affects) && this.client) {
          await this.client.sendNotification(DidChangeConfigurationNotification.type, { settings: null });
        }
      } catch (error) {
        this.output.error(`Failed to apply Pascal LSP configuration change: ${errorMessage(error)}`);
      }
    });
    context.subscriptions.push(this.configListener);
  }

  get status(): ClientStatus {
    return this.currentStatus;
  }

  get client(): LanguageClient | undefined {
    return this.currentStatus === 'running' ? this.activeClient : undefined;
  }

  get binaryPath(): string | undefined {
    return this.currentBinaryPath;
  }

  get startCount(): number {
    return this.successfulStarts;
  }

  get onDidStart(): vscode.Event<LanguageClient> {
    return this.didStartEmitter.event;
  }

  get onDidChangeStatus(): vscode.Event<ClientStatus> {
    return this.didChangeStatusEmitter.event;
  }

  experimental(): Record<string, unknown> {
    const experimental = this.activeClient?.initializeResult?.capabilities.experimental;
    return typeof experimental === 'object' && experimental !== null
      ? experimental as Record<string, unknown>
      : {};
  }

  start(): Promise<void> {
    this.desiredRunning = true;
    return this.enqueue(() => this.startInternal());
  }

  restart(): Promise<void> {
    this.desiredRunning = true;
    return this.enqueue(async () => {
      await this.stopInternal();
      await this.startInternal();
    });
  }

  stop(): Promise<void> {
    this.desiredRunning = false;
    return this.enqueue(() => this.stopInternal());
  }

  dispose(): void {
    this.disposed = true;
    void this.stop().finally(() => {
      this.configListener?.dispose();
      this.stateListener?.dispose();
      this.didStartEmitter.dispose();
      this.didChangeStatusEmitter.dispose();
      this.output.dispose();
    });
  }

  private enqueue(action: () => Promise<void>): Promise<void> {
    const pending = this.lifecycleQueue.run(action);
    void pending.catch((error: unknown) => {
      this.output.error(`Pascal LSP lifecycle operation failed: ${errorMessage(error)}`);
    });
    return pending;
  }

  private async startInternal(): Promise<void> {
    if (this.disposed || !vscode.workspace.isTrusted) return;
    if (this.currentStatus === 'running' || this.currentStatus === 'starting') return;
    if (this.activeClient) await this.stopInternal();

    const resolution = resolveServerBinary(
      configReader()('serverPath') as string | undefined,
      {
        platform: process.platform,
        env: process.env,
        homedir: os.homedir(),
        workspaceFolder: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
        isFile: (candidate) => {
          try {
            return fs.statSync(candidate).isFile();
          } catch {
            return false;
          }
        },
      },
    );

    if (!resolution.ok) {
      this.currentBinaryPath = undefined;
      this.setStatus('failed');
      const message = resolutionMessage(resolution);
      this.output.error(message);
      const choice = await vscode.window.showErrorMessage(message, 'Open Settings');
      if (choice === 'Open Settings') {
        await vscode.commands.executeCommand('workbench.action.openSettings', 'pascalLsp.serverPath');
      }
      return;
    }

    this.currentBinaryPath = resolution.path;
    this.setStatus('starting');

    const configuration = vscode.workspace.getConfiguration('pascalLsp');
    const args = stringArray(configuration.get<unknown>('serverArgs', []));
    const env = stringEnvironment(configuration.get<unknown>('serverEnv', {}));
    const clientRef: { current?: LanguageClient } = {};
    const serverOptions: ServerOptions = {
      command: resolution.path,
      args: ['--stdio', ...args],
      transport: TransportKind.stdio,
      options: { env: { ...process.env, ...env } },
    };

    const clientOptions: LanguageClientOptions = {
      documentSelector: [{ scheme: 'file', language: 'pascal' }],
      outputChannel: this.output,
      initializationOptions: buildInitializationOptions(configReader()),
      middleware: {
        workspace: {
          configuration: async (params, token, next) => {
            const result = await next(params, token);
            if (!Array.isArray(result)) return result;
            return params.items.map((item, index) => (
              item.section === 'pascalLsp' || item.section === 'pascal-lsp'
                ? buildServerSettings(configReader())
                : result[index]
            ));
          },
        },
      },
      errorHandler: {
        error: (_error, _message, count) => ({
          action: (count ?? 0) <= 3 ? ErrorAction.Continue : ErrorAction.Shutdown,
        }),
        closed: () => {
          const client = clientRef.current;
          return client ? this.onClosed(client) : { action: CloseAction.DoNotRestart };
        },
      },
    };

    const client = new LanguageClient('pascalLsp', 'Pascal LSP', serverOptions, clientOptions);
    clientRef.current = client;
    this.activeClient = client;
    this.activeRunStarted = false;
    this.activeRunAnnounced = false;
    this.activeClientReady = false;
    this.closedBeforeStartResolved = false;
    this.activeRunGeneration += 1;
    this.stateListener?.dispose();
    this.stateListener = client.onDidChangeState(({ newState }) => {
      void this.onStateChange(client, newState).catch((error: unknown) => {
        this.output.error(`Failed to update Pascal LSP state: ${errorMessage(error)}`);
      });
    });

    let startResolved = false;
    let startError: unknown;
    try {
      await client.start();
      startResolved = true;
    } catch (error) {
      startError = error;
    }

    if (startOutcome({ closedDuringStart: this.closedBeforeStartResolved, startResolved }) === 'failed') {
      const error = startResolved
        ? new Error('Language client closed before startup completed')
        : startError;
      await this.handleStartFailure(client, error);
      return;
    }

    this.activeClientReady = true;
    this.successfulStarts += 1;
    await this.onStateChange(client, client.state);
  }

  private async handleStartFailure(client: LanguageClient, error: unknown): Promise<void> {
    this.output.error(`Pascal LSP failed to start: ${errorMessage(error)}`);
    this.stateListener?.dispose();
    this.stateListener = undefined;
    if (this.activeClient === client) this.activeClient = undefined;
    this.activeClientReady = false;
    this.closedBeforeStartResolved = false;
    this.activeRunStarted = false;
    this.activeRunAnnounced = false;
    this.activeRunGeneration += 1;
    this.setStatus('failed');
    await this.setCapabilityContexts(undefined);
    const choice = await vscode.window.showErrorMessage(
      `Pascal LSP failed to start: ${errorMessage(error)}`,
      'Show Output',
    );
    if (choice === 'Show Output') this.output.show();
  }

  private async stopInternal(): Promise<void> {
    this.stopRequested = true;
    const client = this.activeClient;
    if (client && this.currentStatus !== 'failed') {
      try {
        await client.stop(STOP_TIMEOUT_MS);
      } catch (error) {
        this.output.error(`Failed to stop Pascal LSP cleanly: ${errorMessage(error)}`);
      }
    }

    this.stateListener?.dispose();
    this.stateListener = undefined;
    if (this.activeClient === client) this.activeClient = undefined;
    this.activeClientReady = false;
    this.closedBeforeStartResolved = false;
    this.activeRunStarted = false;
    this.activeRunAnnounced = false;
    this.activeRunGeneration += 1;
    this.setStatus('stopped');
    await this.setCapabilityContexts(undefined);
    this.stopRequested = false;
  }

  private async onStateChange(client: LanguageClient, state: State): Promise<void> {
    if (this.activeClient !== client) return;

    switch (state) {
      case State.Starting:
        this.activeRunGeneration += 1;
        this.activeRunStarted = false;
        this.activeRunAnnounced = false;
        this.setStatus('starting');
        break;
      case State.Running:
        if (!this.activeRunStarted) {
          this.activeRunStarted = true;
          this.activeRunAnnounced = false;
          this.activeRunGeneration += 1;
        }
        {
          const generation = this.activeRunGeneration;
          await announceClientStarted(
            client,
            () => this.setStatus('running'),
            () => this.setCapabilityContexts(capabilityFlags(this.experimental())),
            (startedClient) => {
              if (
                this.activeClient === client
                && this.activeRunStarted
                && this.activeRunGeneration === generation
                && this.activeClientReady
                && this.desiredRunning
                && !this.stopRequested
                && !this.activeRunAnnounced
              ) {
                this.activeRunAnnounced = true;
                this.didStartEmitter.fire(startedClient);
              }
            },
          );
        }
        break;
      case State.Stopped:
      case State.StartFailed:
        this.activeRunGeneration += 1;
        this.activeRunStarted = false;
        this.activeRunAnnounced = false;
        this.setStatus(this.stopRequested ? 'stopped' : 'failed');
        await this.setCapabilityContexts(undefined);
        break;
    }
  }

  private async onClosed(client: LanguageClient): Promise<CloseHandlerResult> {
    if (this.activeClient === client && !this.activeClientReady && this.desiredRunning && !this.stopRequested) {
      // A close before start() settles is a start failure, whether it resolves or rejects.
      this.closedBeforeStartResolved = true;
      return { action: CloseAction.DoNotRestart };
    }

    if (!shouldRetryClosedClient({
      startResolved: this.activeClientReady,
      desiredRunning: this.desiredRunning,
      stopRequested: this.stopRequested,
      isActive: this.activeClient === client,
    })) {
      return { action: CloseAction.DoNotRestart };
    }

    await this.handleUnexpectedClose(client);
    return { action: CloseAction.DoNotRestart };
  }

  private async handleUnexpectedClose(client: LanguageClient): Promise<void> {
    if (!shouldRetryClosedClient({
      startResolved: this.activeClientReady,
      desiredRunning: this.desiredRunning,
      stopRequested: this.stopRequested,
      isActive: this.activeClient === client,
    })) return;

    this.activeRunStarted = false;
    this.activeRunAnnounced = false;
    this.activeRunGeneration += 1;
    this.setStatus('failed');
    await this.setCapabilityContexts(undefined);
    if (!shouldRetryClosedClient({
      startResolved: this.activeClientReady,
      desiredRunning: this.desiredRunning,
      stopRequested: this.stopRequested,
      isActive: this.activeClient === client,
    })) return;

    if (this.crashWindow.record(Date.now())) {
      this.scheduleClosedRestart(client);
      return;
    }

    const choice = await vscode.window.showErrorMessage(
      'Pascal LSP stopped after repeated crashes.',
      'Restart',
      'Show Output',
    );
    if (choice === 'Restart') {
      this.scheduleClosedRestart(client);
    } else if (choice === 'Show Output') {
      this.output.show();
    }
  }

  private scheduleClosedRestart(client: LanguageClient): void {
    setTimeout(() => {
      void this.enqueue(async () => {
        // Stale retries are no-ops, including timers left behind by a failed start.
        if (this.disposed || !this.desiredRunning || this.activeClient !== client) return;

        this.stateListener?.dispose();
        this.stateListener = undefined;
        this.activeClient = undefined;
        this.activeClientReady = false;
        this.closedBeforeStartResolved = false;
        this.activeRunStarted = false;
        this.activeRunAnnounced = false;
        this.activeRunGeneration += 1;
        await this.startInternal();
      });
    }, 0);
  }

  private setStatus(status: ClientStatus): void {
    if (this.currentStatus === status) return;
    this.currentStatus = status;
    this.didChangeStatusEmitter.fire(status);
  }

  private setCapabilityContexts(flags: Record<ExperimentalFlag, boolean> | undefined): Promise<void> {
    return this.contextQueue.run(async () => {
      await Promise.all(EXPERIMENTAL_FLAGS.map(async (flag) => {
        try {
          await vscode.commands.executeCommand(
            'setContext',
            CAPABILITY_CONTEXT_KEYS[flag],
            flags?.[flag] ?? false,
          );
        } catch (error) {
          this.output.warn(`Failed to update Pascal LSP capability context: ${errorMessage(error)}`);
        }
      }));
    });
  }
}

export function createClientHost(context: vscode.ExtensionContext): ClientHost {
  return new ClientHostImpl(context);
}
