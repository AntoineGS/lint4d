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

export function announceClientStarted<T>(
  client: T,
  setRunning: () => void,
  queueContexts: () => void,
  fireStarted: (client: T) => void,
): void {
  setRunning();
  queueContexts();
  fireStarted(client);
}
