import type { Thread, ThreadReadResult, Turn, TurnStatus } from './types';

const rank: Record<TurnStatus, number> = {
  queued: 0,
  running: 1,
  cancelling: 2,
  completed: 3,
  failed: 3,
  interrupted: 3,
};
/** A delayed snapshot cannot regress a turn, even if the server clock moves back. */
export function mergeTurns(current: Turn[], incoming: Turn[]): Turn[] {
  const map = new Map(current.map((turn) => [turn.id, turn]));
  for (const turn of incoming) {
    const previous = map.get(turn.id);
    if (
      !previous ||
      rank[turn.status] > rank[previous.status] ||
      (turn.status === previous.status && turn.updated_at >= previous.updated_at)
    )
      map.set(turn.id, turn);
  }
  return [...map.values()].sort((a, b) => a.created_at - b.created_at || a.id.localeCompare(b.id));
}
export function mergeThreads(current: Thread[], incoming: Thread[]): Thread[] {
  const map = new Map(current.map((thread) => [thread.id, thread]));
  for (const thread of incoming) {
    const previous = map.get(thread.id);
    if (
      !previous ||
      thread.last_sequence > previous.last_sequence ||
      (thread.last_sequence === previous.last_sequence && thread.updated_at >= previous.updated_at)
    )
      map.set(thread.id, thread);
  }
  return [...map.values()].sort((a, b) => b.updated_at - a.updated_at || a.id.localeCompare(b.id));
}
export function mergeSnapshot(
  previous: ThreadReadResult | null,
  incoming: ThreadReadResult,
): ThreadReadResult {
  if (!previous) return incoming;
  const known = new Set(previous.turns.map((turn) => turn.id));
  const overlaps = incoming.turns.some((turn) => known.has(turn.id));
  // More than a page may arrive while disconnected. Reopen the history frontier
  // when the latest page no longer overlaps, so the intervening turns remain reachable.
  const next_cursor =
    !overlaps && incoming.next_cursor !== null ? incoming.next_cursor : previous.next_cursor;
  return {
    thread: mergeThreads([previous.thread], [incoming.thread])[0],
    turns: mergeTurns(previous.turns, incoming.turns),
    next_cursor,
  };
}

/** Run one snapshot read at a time and retain one refresh requested during it. */
export function coalescedRefresh(
  task: () => Promise<void>,
  signal: AbortSignal,
): () => Promise<void> {
  let running: Promise<void> | undefined;
  let pending = false;
  const drain = async () => {
    try {
      while (pending && !signal.aborted) {
        pending = false;
        await task();
      }
    } finally {
      running = undefined;
    }
  };
  return () => {
    if (signal.aborted) return Promise.resolve();
    pending = true;
    return running ?? (running = drain());
  };
}
