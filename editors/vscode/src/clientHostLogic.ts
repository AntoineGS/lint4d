const CRASH_WINDOW_MS = 3 * 60 * 1000;
const MAX_CRASHES_IN_WINDOW = 5;

export class CrashWindow {
  private closes: number[] = [];

  record(now: number): boolean {
    this.closes = this.closes.filter((closedAt) => now - closedAt <= CRASH_WINDOW_MS);
    this.closes.push(now);
    return this.closes.length < MAX_CRASHES_IN_WINDOW;
  }
}

export class SerialQueue {
  private tail: Promise<void> = Promise.resolve();

  run(action: () => Promise<void>): Promise<void> {
    const pending = this.tail.then(action, action);
    this.tail = pending.then(() => undefined, () => undefined);
    return pending;
  }
}

export function shouldRetryClosedClient(state: {
  startResolved: boolean;
  desiredRunning: boolean;
  stopRequested: boolean;
  isActive: boolean;
}): boolean {
  return state.startResolved && state.desiredRunning && !state.stopRequested && state.isActive;
}

export function startOutcome(state: {
  closedDuringStart: boolean;
  startResolved: boolean;
}): 'running' | 'failed' {
  return state.startResolved && !state.closedDuringStart ? 'running' : 'failed';
}

export function announceClientStarted<T>(
  client: T,
  setRunning: () => void,
  updateContexts: () => Promise<void>,
  fireStarted: (client: T) => void,
): Promise<void> {
  setRunning();
  return updateContexts().then(() => fireStarted(client));
}
