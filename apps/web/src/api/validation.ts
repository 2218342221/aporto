import type {
  AgentEvent,
  AgentListResult,
  AgentSummary,
  RuntimeInstance,
  RuntimeInstanceListResult,
  Thread,
  ThreadListResult,
  ThreadReadResult,
  Turn,
  TurnItem,
  TurnItemListResult,
} from './types';

/** Reject incompatible wire payloads before they enter React state. */
export class ProtocolError extends Error {
  constructor(message = '服务返回了不符合协议的数据') {
    super(message);
    this.name = 'ProtocolError';
  }
}
type RecordValue = Record<string, unknown>;
export type Decoder<T> = (value: unknown) => T;
const object = (value: unknown): value is RecordValue =>
  value !== null && typeof value === 'object' && !Array.isArray(value);
const string = (value: unknown): value is string => typeof value === 'string';
const identifier = (value: unknown) => string(value) && value.length > 0;
const nullableString = (value: unknown) => value === null || string(value);
const sequence = (value: unknown) => Number.isSafeInteger(value) && Number(value) >= 0;
const timestamp = (value: unknown) =>
  Number.isSafeInteger(value) && Math.abs(Number(value)) <= 8.64e15;
const cursor = (value: unknown) => value === null || identifier(value);
const statuses = new Set(['queued', 'running', 'cancelling', 'completed', 'failed', 'interrupted']);
function decode<T>(value: unknown, valid: (value: unknown) => boolean): T {
  if (!valid(value)) throw new ProtocolError();
  return value as T;
}
const unique = (values: Array<{ id: string }>) =>
  new Set(values.map((value) => value.id)).size === values.length;
const isAgent = (value: unknown): value is AgentSummary =>
  object(value) &&
  identifier(value.id) &&
  string(value.name) &&
  string(value.model) &&
  string(value.bundle_digest) &&
  string(value.release_id) &&
  object(value.runtime) &&
  ['docker', 'agentenv'].includes(String(value.runtime.provider)) &&
  Array.isArray(value.runtime.images) &&
  value.runtime.images.length > 0 &&
  value.runtime.images.every(identifier) &&
  new Set(value.runtime.images).size === value.runtime.images.length &&
  identifier(value.runtime.default_image) &&
  value.runtime.images.includes(value.runtime.default_image) &&
  identifier(value.runtime.default_workdir);
const isThread = (value: unknown): value is Thread =>
  object(value) &&
  identifier(value.id) &&
  string(value.title) &&
  identifier(value.agent_id) &&
  string(value.bundle_digest) &&
  string(value.release_id) &&
  nullableString(value.sandbox_id) &&
  identifier(value.runtime_instance_id) &&
  identifier(value.runtime_image) &&
  identifier(value.runtime_provider) &&
  identifier(value.workdir) &&
  timestamp(value.created_at) &&
  timestamp(value.updated_at) &&
  sequence(value.last_sequence);
const isTurn = (value: unknown): value is Turn =>
  object(value) &&
  identifier(value.id) &&
  identifier(value.thread_id) &&
  string(value.input) &&
  string(value.status) &&
  statuses.has(value.status) &&
  nullableString(value.output) &&
  nullableString(value.error) &&
  timestamp(value.created_at) &&
  timestamp(value.updated_at);

const isInstance = (value: unknown): value is RuntimeInstance =>
  object(value) &&
  identifier(value.id) &&
  identifier(value.agent_id) &&
  identifier(value.release_id) &&
  identifier(value.bundle_digest) &&
  identifier(value.provider) &&
  identifier(value.image) &&
  identifier(value.sandbox_id) &&
  identifier(value.workdir) &&
  typeof value.busy === 'boolean' &&
  timestamp(value.created_at) &&
  timestamp(value.updated_at);

export const instanceList =
  (agentId: string, after?: string): Decoder<RuntimeInstanceListResult> =>
  (value) =>
    decode(
      value,
      (v) =>
        object(v) &&
        Array.isArray(v.instances) &&
        v.instances.length <= 50 &&
        v.instances.every((instance) => isInstance(instance) && instance.agent_id === agentId) &&
        unique(v.instances) &&
        cursor(v.next_cursor) &&
        (v.next_cursor === null || v.next_cursor !== after),
    );

const itemKinds = new Set(['assistant_message', 'ptc_call', 'tool_call']);
const itemStatuses = new Set(['in_progress', 'completed', 'failed', 'interrupted']);
export const isItemEvent = (kind: unknown) =>
  string(kind) && ['item.started', 'item.updated', 'item.completed'].includes(kind);
const isTurnItem = (value: unknown): value is TurnItem =>
  object(value) &&
  identifier(value.id) &&
  identifier(value.turn_id) &&
  sequence(value.ordinal) &&
  Number(value.ordinal) > 0 &&
  sequence(value.sequence) &&
  Number(value.sequence) >= Number(value.ordinal) &&
  timestamp(value.created_at) &&
  timestamp(value.updated_at) &&
  string(value.kind) &&
  itemKinds.has(value.kind) &&
  string(value.status) &&
  itemStatuses.has(value.status) &&
  (value.phase === null || value.phase === 'commentary' || value.phase === 'final_answer') &&
  nullableString(value.name) &&
  nullableString(value.text) &&
  nullableString(value.input) &&
  nullableString(value.output) &&
  nullableString(value.error) &&
  (value.elapsed_ms === null || sequence(value.elapsed_ms)) &&
  typeof value.truncated === 'boolean';

export const turnItemList =
  (turnId: string, after: number): Decoder<TurnItemListResult> =>
  (value) =>
    decode(
      value,
      (v) =>
        object(v) &&
        Array.isArray(v.items) &&
        v.items.length <= 100 &&
        v.items.every(
          (item, index, items) =>
            isTurnItem(item) &&
            item.turn_id === turnId &&
            item.ordinal > after &&
            (index === 0 || item.ordinal > items[index - 1].ordinal),
        ) &&
        unique(v.items) &&
        sequence(v.next_cursor) &&
        v.next_cursor === (v.items.length ? v.items[v.items.length - 1].ordinal : after) &&
        typeof v.has_more === 'boolean' &&
        (!v.has_more || v.items.length > 0),
    );

/** Events carry full cumulative snapshots, never text deltas or reasoning. */
export const eventItem = (event: AgentEvent): TurnItem | null =>
  isItemEvent(event.kind) ? (event.data as { item: TurnItem }).item : null;

export const agentList: Decoder<AgentListResult> = (value) =>
  decode(
    value,
    (v) => object(v) && Array.isArray(v.agents) && v.agents.every(isAgent) && unique(v.agents),
  );
export const threadList: Decoder<ThreadListResult> = (value) =>
  decode(
    value,
    (v) =>
      object(v) &&
      Array.isArray(v.threads) &&
      v.threads.every(isThread) &&
      unique(v.threads) &&
      cursor(v.next_cursor),
  );
export const thread: Decoder<Thread> = (value) => decode(value, isThread);
export const threadRead =
  (id: string): Decoder<ThreadReadResult> =>
  (value) =>
    decode(
      value,
      (v) =>
        object(v) &&
        isThread(v.thread) &&
        v.thread.id === id &&
        Array.isArray(v.turns) &&
        v.turns.every((t) => isTurn(t) && t.thread_id === id) &&
        unique(v.turns) &&
        cursor(v.next_cursor),
    );
export const turn =
  (threadId: string, turnId?: string): Decoder<Turn> =>
  (value) =>
    decode(
      value,
      (v) => isTurn(v) && v.thread_id === threadId && (turnId === undefined || v.id === turnId),
    );
export const agentEvent =
  (threadId: string): Decoder<AgentEvent> =>
  (value) =>
    decode(
      value,
      (v) =>
        object(v) &&
        sequence(v.sequence) &&
        Number(v.sequence) > 0 &&
        v.thread_id === threadId &&
        nullableString(v.turn_id) &&
        identifier(v.kind) &&
        'data' in v &&
        (!isItemEvent(v.kind) ||
          (object(v.data) &&
            isTurnItem(v.data.item) &&
            v.data.item.turn_id === v.turn_id &&
            v.data.item.sequence === v.sequence)) &&
        timestamp(v.created_at),
    );
export const errorEnvelope: Decoder<{ error: { code: number; message: string } }> = (value) =>
  decode(
    value,
    (v) =>
      object(v) && object(v.error) && Number.isInteger(v.error.code) && string(v.error.message),
  );
