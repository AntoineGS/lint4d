import { describe, expect, it } from 'vitest';
import { announceClientStarted, CrashWindow, SerialQueue } from '../../src/clientHostLogic';

describe('announceClientStarted', () => {
  it('sets running and queues capability contexts before firing the start event', () => {
    let status = 'starting';
    let contextsReady = false;
    const order: string[] = [];
    const getClient = () => status === 'running' ? 'client' : undefined;

    announceClientStarted(
      'client',
      () => {
        status = 'running';
        order.push('status');
      },
      () => {
        contextsReady = true;
        order.push('contexts');
      },
      (client) => {
        expect(client).toBe('client');
        expect(getClient()).toBe('client');
        expect(contextsReady).toBe(true);
        order.push('event');
      },
    );

    expect(order).toEqual(['status', 'contexts', 'event']);
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
