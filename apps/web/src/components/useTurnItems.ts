import { useCallback, useEffect, useReducer, useRef } from 'react';
import type { AgentClient } from '../api/client';
import type { AgentEvent, Turn } from '../api/types';
import { eventItem } from '../api/validation';
import { itemsReducer } from '../api/items';

/** Keeps the transcript independent from thread snapshots and their pagination. */
export function useTurnItems(
  client: AgentClient,
  threadId: string,
  turns: Turn[],
  connectionEpoch: number,
) {
  const [state, dispatch] = useReducer(itemsReducer, {});
  const stateRef = useRef(state);
  stateRef.current = state;
  const controllerRef = useRef<AbortController | null>(null);
  const pending = useRef(new Map<string, Promise<void>>());
  const refreshPending = useRef(new Set<string>());
  const tracked = useRef(new Set<string>());
  tracked.current = new Set(turns.map((turn) => turn.id));
  const turnKeys = JSON.stringify(turns.map((turn) => turn.id));

  useEffect(() => {
    const controller = new AbortController();
    controllerRef.current = controller;
    return () => {
      controller.abort();
      pending.current.clear();
      refreshPending.current.clear();
    };
  }, [client, threadId]);

  const readPage = useCallback(
    (turnId: string, after: number): Promise<void> => {
      const controller = controllerRef.current;
      if (!controller || controller.signal.aborted) return Promise.resolve();
      const running = pending.current.get(turnId);
      if (running) {
        if (after === 0) refreshPending.current.add(turnId);
        return running;
      }
      dispatch({ type: 'loading', turnId });
      const task = (async () => {
        try {
          const page = await client.items(threadId, turnId, controller.signal, after);
          if (!controller.signal.aborted) dispatch({ type: 'page', turnId, after, page });
        } catch (error) {
          if (!controller.signal.aborted)
            dispatch({
              type: 'error',
              turnId,
              error: error instanceof Error ? error.message : '活动加载失败',
            });
        } finally {
          if (!controller.signal.aborted) {
            pending.current.delete(turnId);
            if (refreshPending.current.delete(turnId)) void readPage(turnId, 0);
          }
        }
      })();
      pending.current.set(turnId, task);
      return task;
    },
    [client, threadId],
  );

  useEffect(() => {
    // New turns need a durable starting point, including events received before
    // the thread snapshot made that turn visible. Existing history stays loaded.
    for (const turnId of JSON.parse(turnKeys) as string[])
      if (!stateRef.current[turnId]?.loaded) void readPage(turnId, 0);
  }, [turnKeys, readPage]);

  useEffect(() => {
    if (!connectionEpoch) return;
    for (const turnId of tracked.current) void readPage(turnId, 0);
  }, [connectionEpoch, readPage]);

  const receive = useCallback((event: AgentEvent) => {
    const item = eventItem(event);
    // Replayed events for unloaded old turns do not expand the history in memory.
    // Loading that turn later restores its complete first page from durable state.
    if (item && tracked.current.has(item.turn_id)) dispatch({ type: 'event', item });
  }, []);
  const loadMore = useCallback(
    (turnId: string) => readPage(turnId, stateRef.current[turnId]?.cursor ?? 0),
    [readPage],
  );
  const retry = useCallback((turnId: string) => readPage(turnId, 0), [readPage]);
  return { state, receive, loadMore, retry };
}
