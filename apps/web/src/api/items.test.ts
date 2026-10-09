import { describe, expect, it, vi } from 'vitest';
import { itemsReducer, mergeItems } from './items';
import type { ItemsState } from './items';
import type { TurnItem } from './types';
import { AgentClient } from './client';
import { agentEvent, ProtocolError, turnItemList } from './validation';

const item = (id: string, ordinal: number, overrides: Partial<TurnItem> = {}): TurnItem => ({
  id,
  turn_id: 'turn',
  ordinal,
  sequence: ordinal,
  created_at: 1,
  updated_at: 1,
  kind: 'assistant_message',
  status: 'in_progress',
  phase: 'commentary',
  name: null,
  text: '开始检查',
  input: null,
  output: null,
  error: null,
  elapsed_ms: null,
  truncated: false,
  ...overrides,
});

describe('activity snapshot reconciliation', () => {
  it('keeps cumulative streamed text against an older REST snapshot and repeated replay', () => {
    const started = item('message', 2);
    const updated = { ...started, sequence: 9, text: '开始检查依赖和配置。', updated_at: 0 };
    let state: ItemsState = {};
    state = itemsReducer(state, { type: 'event', item: updated });
    state = itemsReducer(state, {
      type: 'page',
      turnId: 'turn',
      after: 0,
      page: { items: [started], next_cursor: 2, has_more: false },
    });
    expect(state.turn.items).toEqual([updated]);
    expect(itemsReducer(state, { type: 'event', item: updated })).toBe(state);
    const completed = { ...updated, sequence: 10, status: 'completed' as const };
    expect(mergeItems([completed], [updated])).toEqual([completed]);
  });

  it('retains creation order when parallel tool calls finish in reverse order', () => {
    const first = item('first', 3, { kind: 'tool_call', phase: null });
    const second = item('second', 4, { kind: 'tool_call', phase: null });
    expect(
      mergeItems([second], [{ ...first, sequence: 7, status: 'completed' }]).map(
        (value) => value.id,
      ),
    ).toEqual(['first', 'second']);
  });

  it('does not let a live item skip unloaded activity pages or erase loaded history', () => {
    let state: ItemsState = {};
    state = itemsReducer(state, {
      type: 'page',
      turnId: 'turn',
      after: 0,
      page: { items: [item('first', 1)], next_cursor: 1, has_more: true },
    });
    state = itemsReducer(state, { type: 'event', item: item('live', 10) });
    expect(state.turn.cursor).toBe(1);
    state = itemsReducer(state, {
      type: 'page',
      turnId: 'turn',
      after: 1,
      page: { items: [item('middle', 5), item('live', 10)], next_cursor: 10, has_more: false },
    });
    state = itemsReducer(state, {
      type: 'page',
      turnId: 'turn',
      after: 0,
      page: { items: [item('first', 1)], next_cursor: 1, has_more: true },
    });
    expect(state.turn.items.map((value) => value.id)).toEqual(['first', 'middle', 'live']);
    expect(state.turn.cursor).toBe(10);
    state = itemsReducer(state, {
      type: 'page',
      turnId: 'turn',
      after: 10,
      page: { items: [], next_cursor: 10, has_more: false },
    });
    expect(state.turn.hasMore).toBe(false);
  });

  it('keeps partial interrupted messages and errors when a stale in-progress snapshot arrives', () => {
    const interrupted = item('partial', 1, {
      sequence: 8,
      status: 'interrupted',
      text: '已检查一部分',
      error: '任务已中断',
    });
    expect(mergeItems([interrupted], [item('partial', 1)])).toEqual([interrupted]);
    const state = itemsReducer(
      {
        turn: {
          items: [interrupted],
          cursor: 1,
          hasMore: true,
          loading: true,
          loaded: true,
          error: '',
        },
      },
      { type: 'error', turnId: 'turn', error: '离线' },
    );
    expect(state.turn.items).toEqual([interrupted]);
    expect(state.turn.loading).toBe(false);
    expect(state.turn.hasMore).toBe(true);
  });
});

describe('activity wire contract', () => {
  it('encodes both identifiers and uses the immutable ordinal for pagination', async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(
        JSON.stringify({
          items: [item('next', 8, { turn_id: 'turn/1', sequence: 30 })],
          next_cursor: 8,
          has_more: false,
        }),
      ),
    );
    const client = new AgentClient('http://localhost', 'token', fetcher);
    await client.items('thread/1', 'turn/1', undefined, 7);
    expect(fetcher.mock.calls[0][0]).toBe(
      'http://localhost/v1/threads/thread%2F1/turns/turn%2F1/items?after=7&limit=100',
    );
  });

  it('rejects invalid item identity, cursors, status and untyped event payloads', () => {
    const valid = item('message', 2);
    for (const invalid of [
      { ...valid, turn_id: 'other' },
      { ...valid, status: 'running' },
      { ...valid, ordinal: 0 },
      { ...valid, sequence: 1 },
      { ...valid, phase: 'reasoning' },
      { ...valid, elapsed_ms: -1 },
      { ...valid, text: { secret: 'data' } },
      { ...valid, truncated: 'false' },
    ])
      expect(() =>
        turnItemList('turn', 0)({ items: [invalid], next_cursor: 2, has_more: false }),
      ).toThrow(ProtocolError);
    for (const page of [
      { items: [valid], next_cursor: 9, has_more: false },
      { items: [valid, valid], next_cursor: 2, has_more: false },
      { items: [item('later', 4), valid], next_cursor: 2, has_more: false },
      { items: [], next_cursor: 0, has_more: true },
    ])
      expect(() => turnItemList('turn', 0)(page)).toThrow(ProtocolError);
    const event = {
      sequence: 2,
      thread_id: 'thread',
      turn_id: 'turn',
      kind: 'item.started',
      created_at: 1,
      data: { item: valid },
    };
    expect(agentEvent('thread')(event)).toEqual(event);
    for (const changed of [{ data: {} }, { sequence: 3 }, { turn_id: 'other' }])
      expect(() => agentEvent('thread')({ ...event, ...changed })).toThrow(ProtocolError);
  });
});
