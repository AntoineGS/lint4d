import { describe, expect, it } from 'vitest';
import {
  announceClientStarted,
  CrashWindow,
  SerialQueue,
  startOutcome,
  shouldRetryClosedClient,
} from '../../src/clientHostLogic';

describe('announceClientStarted', () => {
  it('awaits capability updates before firing the start event', async () => {
    let status = 'starting';
    let contextsReady = false;
    const order: string[] = [];
    const getClient = () => status === 'running' ? 'client' : undefined;
    let finishContexts!: () => void;
    const contextsUpdated = new Promise<void>((resolve) => { finishContexts = resolve; });

    const announcement = announceClientStarted(
      'client',
      () => {
        status = 'running';
        order.push('status');
      },
      () => {
        order.push('contexts');
        return contextsUpdated.then(() => { contextsReady = true; });
      },
      (client) => {
        expect(client).toBe('client');
        expect(getClient()).toBe('client');
        expect(contextsReady).toBe(true);
        order.push('event');
      },
    );

    expect(order).toEqual(['status', 'contexts']);
    finishContexts();
    await announcement;
    expect(order).toEqual(['status', 'contexts', 'event']);
  });
});

describe('shouldRetryClosedClient', () => {
  it('does not retry a close before start resolves or after a manual stop', () => {
    const readyClient = {
      startResolved: true,
      desiredRunning: true,
      stopRequested: false,
      isActive: true,
    };
    expect(shouldRetryClosedClient({ ...readyClient, startResolved: false })).toBe(false);
    expect(shouldRetryClosedClient({ ...readyClient, stopRequested: true })).toBe(false);
    expect(shouldRetryClosedClient(readyClient)).toBe(true);
  });
});

describe('startOutcome', () => {
  it('routes only resolved starts without an early close to running', () => {
    expect(startOutcome({ closedDuringStart: false, startResolved: true })).toBe('running');
    expect(startOutcome({ closedDuringStart: true, startResolved: true })).toBe('failed');
    expect(startOutcome({ closedDuringStart: false, startResolved: false })).toBe('failed');
  });
});

describe('CrashWindow', () => {
  it('allows four restarts in three minutes and blocks the fifth', () => {
    const crashes = new CrashWindow();
    expect([0, 1, 2, 3, 4].map((time) => crashes.record(time)))
      .toEqual([true, true, true, true, false]);
    expect(crashes.record(180_005)).toBe(true);
  });
});

describe('SerialQueue', () => {
  it('keeps lifecycle work serialized and continues after a rejected action', async () => {
    const queue = new SerialQueue();
    const order: string[] = [];
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const first = queue.run(async () => {
      order.push('first:start');
      await gate;
      order.push('first:end');
    });
    const second = queue.run(async () => { order.push('second'); });

    await Promise.resolve();
    expect(order).toEqual(['first:start']);
    release();
    await Promise.all([first, second]);
    expect(order).toEqual(['first:start', 'first:end', 'second']);

    const failure = queue.run(async () => { throw new Error('expected failure'); });
    const afterFailure = queue.run(async () => { order.push('after failure'); });
    await expect(failure).rejects.toThrow('expected failure');
    await afterFailure;
    expect(order.at(-1)).toBe('after failure');
  });
});
