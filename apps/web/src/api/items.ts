import type { TurnItem, TurnItemListResult } from './types';

export interface TurnItemsState {
  items: TurnItem[];
  cursor: number;
  hasMore: boolean;
  loaded: boolean;
  loading: boolean;
  error: string;
}
export type ItemsState = Record<string, TurnItemsState>;
export const emptyTurnItems = (): TurnItemsState => ({
  items: [],
  cursor: 0,
  hasMore: false,
  loaded: false,
  loading: false,
  error: '',
});

/** REST and SSE carry cumulative snapshots. Sequence, not arrival order, wins. */
export function mergeItems(current: TurnItem[], incoming: TurnItem[]): TurnItem[] {
  const items = new Map(current.map((item) => [item.id, item]));
  let changed = false;
  for (const item of incoming) {
    const previous = items.get(item.id);
    if (!previous || item.sequence > previous.sequence) {
      items.set(item.id, item);
      changed = true;
    }
  }
  return changed
    ? [...items.values()].sort((a, b) => a.ordinal - b.ordinal || a.id.localeCompare(b.id))
    : current;
}

export type ItemsAction =
  | { type: 'event'; item: TurnItem }
  | { type: 'loading'; turnId: string }
  | { type: 'page'; turnId: string; after: number; page: TurnItemListResult }
  | { type: 'error'; turnId: string; error: string };

export function itemsReducer(state: ItemsState, action: ItemsAction): ItemsState {
  const turnId = action.type === 'event' ? action.item.turn_id : action.turnId;
  const previous = state[turnId] ?? emptyTurnItems();
  let next: TurnItemsState;
  switch (action.type) {
    case 'event': {
      const items = mergeItems(previous.items, [action.item]);
      if (items === previous.items) return state;
      // An SSE item can be beyond a snapshot gap. It must not advance pagination.
      next = { ...previous, items };
      break;
    }
    case 'loading':
      next = { ...previous, loading: true, error: '' };
      break;
    case 'error':
      next = { ...previous, loading: false, error: action.error };
      break;
    case 'page': {
      const advances = action.page.next_cursor >= previous.cursor || !previous.loaded;
      next = {
        ...previous,
        items: mergeItems(previous.items, action.page.items),
        // A first-page reconnect refresh must retain every manually loaded page.
        cursor: Math.max(previous.cursor, action.page.next_cursor),
        hasMore: advances ? action.page.has_more : previous.hasMore,
        loaded: true,
        loading: false,
        error: '',
      };
      break;
    }
  }
  return { ...state, [turnId]: next };
}
