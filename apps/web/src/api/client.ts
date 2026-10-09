import { SseParser } from './events';
import * as wire from './validation';
import type {
  AgentEvent,
  AgentListResult,
  ConnectionState,
  Thread,
  ThreadListResult,
  ThreadReadResult,
  Turn,
  TurnItemListResult,
  RuntimeInstanceListResult,
  ThreadCreateInput,
} from './types';

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

export function normalizeBaseUrl(value: string): string {
  const url = new URL(value.trim());
  if (
    !['http:', 'https:'].includes(url.protocol) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  ) {
    throw new Error('请填写 HTTP(S) 服务地址，不包含账号、查询参数或锚点');
  }
  return url.toString().replace(/\/+$/, '');
}

export interface StreamOptions {
  signal: AbortSignal;
  after?: number;
  onEvent: (event: AgentEvent) => void;
  onState: (state: ConnectionState) => void;
  onError: (error: Error, fatal: boolean) => void;
  reconnectDelayMs?: number;
}

function abortableDelay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve();
    const finish = () => {
      clearTimeout(timer);
      signal.removeEventListener('abort', finish);
      resolve();
    };
    const timer = setTimeout(finish, ms);
    signal.addEventListener('abort', finish, { once: true });
  });
}

async function responseError(response: Response): Promise<ApiError> {
  let message = `服务请求失败（HTTP ${response.status}）`;
  try {
    const data: unknown = await response.json();
    if (data && typeof data === 'object' && 'error' in data) {
      const error = data.error;
      if (
        error &&
        typeof error === 'object' &&
        'message' in error &&
        typeof error.message === 'string'
      )
        message = error.message;
      else if (typeof error === 'string') message = error;
    }
  } catch {
    /* Keep the status when an upstream proxy returns non-JSON. */
  }
  return new ApiError(response.status, message);
}

function streamError(value: unknown): ApiError {
  const { code, message } = wire.errorEnvelope(value).error;
  const statuses: Record<number, number> = {
    [-32001]: 401,
    [-32003]: 403,
    [-32004]: 404,
    [-32009]: 409,
    [-32600]: 400,
    [-32602]: 400,
    [-32010]: 503,
    [-32029]: 429,
  };
  return new ApiError(statuses[code] ?? 502, message);
}

export class AgentClient {
  readonly baseUrl: string;
  constructor(
    baseUrl: string,
    private readonly token: string,
    private readonly fetcher: typeof fetch = (...args) => globalThis.fetch(...args),
  ) {
    this.baseUrl = normalizeBaseUrl(baseUrl);
  }

  private headers(extra?: HeadersInit): Headers {
    const headers = new Headers(extra);
    if (this.token) headers.set('Authorization', `Bearer ${this.token}`);
    return headers;
  }

  private async request<T>(
    path: string,
    decode: wire.Decoder<T>,
    signal?: AbortSignal,
    body?: unknown,
  ): Promise<T> {
    const response = await this.fetcher(`${this.baseUrl}${path}`, {
      method: body === undefined ? 'GET' : 'POST',
      signal,
      credentials: 'omit',
      headers: this.headers(
        body === undefined
          ? { Accept: 'application/json' }
          : { Accept: 'application/json', 'Content-Type': 'application/json' },
      ),
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    if (!response.ok) throw await responseError(response);
    let value: unknown;
    try {
      value = await response.json();
    } catch (error) {
      if (signal?.aborted) throw error;
      throw new wire.ProtocolError('服务返回了无效的 JSON');
    }
    return decode(value);
  }

  agents(signal?: AbortSignal) {
    return this.request<AgentListResult>('/v1/agents', wire.agentList, signal);
  }
  threads(signal?: AbortSignal, cursor?: string) {
    const query = new URLSearchParams({ limit: '50' });
    if (cursor) query.set('cursor', cursor);
    return this.request<ThreadListResult>(`/v1/threads?${query}`, wire.threadList, signal);
  }
  createThread(input: ThreadCreateInput, signal?: AbortSignal) {
    return this.request<Thread>('/v1/threads', wire.thread, signal, {
      ...input,
      title: input.title || null,
    });
  }
  instances(agentId: string, signal?: AbortSignal, cursor?: string) {
    const query = new URLSearchParams({ limit: '50' });
    if (cursor) query.set('cursor', cursor);
    return this.request<RuntimeInstanceListResult>(
      `/v1/agents/${encodeURIComponent(agentId)}/instances?${query}`,
      wire.instanceList(agentId, cursor),
      signal,
    );
  }
  thread(id: string, signal?: AbortSignal, before?: string) {
    const query = new URLSearchParams({ limit: '20' });
    if (before) query.set('before', before);
    return this.request<ThreadReadResult>(
      `/v1/threads/${encodeURIComponent(id)}?${query}`,
      wire.threadRead(id),
      signal,
    );
  }
  startTurn(threadId: string, input: string, idempotencyKey: string, signal?: AbortSignal) {
    return this.request<Turn>(
      `/v1/threads/${encodeURIComponent(threadId)}/turns`,
      wire.turn(threadId),
      signal,
      {
        input,
        idempotency_key: idempotencyKey,
      },
    );
  }
  items(threadId: string, turnId: string, signal?: AbortSignal, after = 0) {
    if (!Number.isSafeInteger(after) || after < 0) throw new wire.ProtocolError('无效的活动游标');
    const query = new URLSearchParams({ after: String(after), limit: '100' });
    return this.request<TurnItemListResult>(
      `/v1/threads/${encodeURIComponent(threadId)}/turns/${encodeURIComponent(turnId)}/items?${query}`,
      wire.turnItemList(turnId, after),
      signal,
    );
  }
  interrupt(threadId: string, turnId: string, signal?: AbortSignal) {
    return this.request<Turn>(
      `/v1/threads/${encodeURIComponent(threadId)}/turns/${encodeURIComponent(turnId)}/interrupt`,
      wire.turn(threadId, turnId),
      signal,
      {},
    );
  }

  async events(threadId: string, options: StreamOptions): Promise<void> {
    const { signal, onEvent, onState, onError } = options;
    let cursor = options.after ?? 0;
    if (!Number.isSafeInteger(cursor) || cursor < 0) {
      onError(new wire.ProtocolError('无效的事件游标'), true);
      onState('disconnected');
      return;
    }
    let failures = 0;
    while (!signal.aborted) {
      onState(failures ? 'reconnecting' : 'connecting');
      let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
      try {
        const response = await this.fetcher(
          `${this.baseUrl}/v1/threads/${encodeURIComponent(threadId)}/events?after=${cursor}`,
          {
            signal,
            credentials: 'omit',
            headers: this.headers({ Accept: 'text/event-stream', 'Last-Event-ID': String(cursor) }),
          },
        );
        if (!response.ok) throw await responseError(response);
        if (
          !response.headers.get('content-type')?.includes('text/event-stream') ||
          !response.body
        ) {
          throw new ApiError(422, '服务没有返回 SSE 事件流');
        }
        if (signal.aborted) break;
        onState('connected');
        const decoder = new TextDecoder();
        const parser = new SseParser((message) => {
          if (signal.aborted) return;
          if (!['agent_event', 'server_error'].includes(message.event)) return;
          let value: unknown;
          try {
            value = JSON.parse(message.data);
          } catch {
            throw new wire.ProtocolError('收到无效的事件 JSON');
          }
          if (message.event === 'server_error') throw streamError(value);
          const event = wire.agentEvent(threadId)(value);
          if (message.id !== String(event.sequence))
            throw new wire.ProtocolError('事件 ID 与游标不一致');
          if (event.sequence <= cursor) return;
          onEvent(event);
          cursor = event.sequence;
          failures = 0;
        });
        reader = response.body.getReader();
        while (!signal.aborted) {
          const { done, value } = await reader.read();
          if (done) {
            parser.feed(decoder.decode());
            break;
          }
          parser.feed(decoder.decode(value, { stream: true }));
        }
      } catch (error) {
        if (signal.aborted) break;
        const failure = error instanceof Error ? error : new Error('事件连接中断');
        const fatal =
          failure instanceof wire.ProtocolError ||
          (failure instanceof ApiError &&
            failure.status >= 400 &&
            failure.status < 500 &&
            ![408, 429].includes(failure.status));
        onError(failure, fatal);
        if (fatal) {
          onState('disconnected');
          return;
        }
      } finally {
        if (reader) {
          await reader.cancel().catch(() => {});
          reader.releaseLock();
        }
      }
      if (signal.aborted) break;
      failures++;
      onState('reconnecting');
      await abortableDelay(
        Math.min((options.reconnectDelayMs ?? 750) * 2 ** Math.min(failures - 1, 4), 12_000),
        signal,
      );
    }
  }
}
