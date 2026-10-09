import { describe, expect, it, vi } from 'vitest';
import { coalescedRefresh, mergeSnapshot, mergeThreads, mergeTurns } from './state';
import type { Thread, Turn } from './types';

const thread: Thread = {
  id: 'one',
  title: 'Title',
  agent_id: 'agent',
  bundle_digest: 'bundle',
  release_id: 'bundle',
  sandbox_id: null,
  runtime_instance_id: 'instance',
  runtime_image: 'node:22',
  runtime_provider: 'docker',
  workdir: '/workspace',
  created_at: 1,
  updated_at: 1,
  last_sequence: 1,
};
const turn: Turn = {
  id: 'old',
  thread_id: 'one',
  input: 'hi',
  status: 'completed',
  output: 'done',
  error: null,
  created_at: 1,
  updated_at: 1,
};

describe('durable snapshot reconciliation', () => {
  it('keeps newer thread metadata when an older pagination response arrives', () => {
    const latest = { ...thread, last_sequence: 10, updated_at: 10, sandbox_id: 'ready' };
    expect(mergeThreads([latest], [thread])).toEqual([latest]);
  });
  it('reopens a history gap after more than one page arrives during a disconnect', () => {
    const previous = { thread, turns: [turn], next_cursor: null };
    const newer = { ...turn, id: 'latest', created_at: 30, updated_at: 30 };
    const incoming = {
      thread: { ...thread, last_sequence: 30 },
      turns: [newer],
      next_cursor: 'missing-middle',
    };
    const result = mergeSnapshot(previous, incoming);
    expect(result.turns.map((t) => t.id)).toEqual(['old', 'latest']);
    expect(result.next_cursor).toBe('missing-middle');
    expect(mergeSnapshot({ ...result, next_cursor: null }, incoming).next_cursor).toBeNull();
  });
  it('allows lifecycle progress when wall clock moves backwards without accepting a stale status', () => {
    const running = { ...turn, status: 'running' as const, updated_at: 10, output: null };
    expect(mergeTurns([running], [turn])).toEqual([turn]);
    expect(mergeTurns([turn], [running])).toEqual([turn]);
  });
});

it('coalesces arbitrarily many refreshes into one running read and one pending read', async () => {
  const release: Array<() => void> = [];
  let active = 0;
  let maximum = 0;
  const task = vi.fn(async () => {
    maximum = Math.max(maximum, ++active);
    await new Promise<void>((resolve) => release.push(resolve));
    active--;
  });
  const stop = new AbortController();
  const refresh = coalescedRefresh(task, stop.signal);
  const done = refresh();
  for (let i = 0; i < 100; i++) void refresh();
  expect(task).toHaveBeenCalledTimes(1);
  release.shift()!();
  await vi.waitFor(() => expect(task).toHaveBeenCalledTimes(2));
  stop.abort();
  void refresh();
  release.shift()!();
  await done;
  expect(task).toHaveBeenCalledTimes(2);
  expect(maximum).toBe(1);
});
