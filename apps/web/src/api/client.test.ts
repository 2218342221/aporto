import { describe, expect, it, vi } from 'vitest';
import { AgentClient, normalizeBaseUrl } from './client';
import { SseParser } from './events';
import { mergeTurns } from './state';
import { ProtocolError } from './validation';
import type { AgentEvent, Turn } from './types';

const event = (sequence: number, thread_id = 'thread 1'): AgentEvent => ({
  sequence,
  thread_id,
  turn_id: null,
  kind: 'turn.started',
  data: {},
  created_at: 1_700_000_000_000,
});
const frame = (sequence: number, thread?: string) =>
  `id: ${sequence}\nevent: agent_event\ndata: ${JSON.stringify(event(sequence, thread))}\n\n`;
const stream = (chunks: string[]) =>
  new Response(
    new ReadableStream({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(new TextEncoder().encode(chunk));
        controller.close();
      },
    }),
    { headers: { 'Content-Type': 'text/event-stream' } },
  );

describe('SSE frame parsing', () => {
  it('handles every CRLF split boundary, comments and multiline data', () => {
    const input =
      ': heartbeat\r\nid: 18\r\nevent: agent_event\r\ndata: {"a":\r\ndata: "中文"}\r\n\r\n';
    for (let split = 1; split < input.length; split++) {
      const receive = vi.fn();
      const parser = new SseParser(receive);
      parser.feed(input.slice(0, split));
      parser.feed(input.slice(split));
      expect(receive).toHaveBeenCalledExactlyOnceWith({
        id: '18',
        event: 'agent_event',
        data: '{"a":\n"中文"}',
      });
    }
  });
  it('rejects unbounded frames and does not dispatch an incomplete event', () => {
    const receive = vi.fn();
    const parser = new SseParser(receive, 30);
    parser.feed('data: not finished');
    expect(receive).not.toHaveBeenCalled();
    expect(() => parser.feed('x'.repeat(40))).toThrow('超过客户端限制');
    const repeated = new SseParser(receive, 30);
    expect(() => repeated.feed('data: x\ndata: x\ndata: x\ndata: x\n')).toThrow();
    expect(() => new SseParser(receive, 20).feed('data: 中文中文中文')).toThrow();
  });
  it('supports bare CR endings and ignores NUL event ids', () => {
    const receive = vi.fn();
    const parser = new SseParser(receive);
    parser.feed('id: no\0pe\rdata: hello\r\r');
    expect(receive).toHaveBeenCalledExactlyOnceWith({
      id: undefined,
      event: 'message',
      data: 'hello',
    });
  });
});

describe('authenticated API client', () => {
  it('uses authorization headers, encoded paths and snake_case idempotency fields', async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(
        JSON.stringify({
          id: 'turn',
          thread_id: 'a/b',
          input: 'inspect files',
          status: 'queued',
          output: null,
          error: null,
          created_at: 1,
          updated_at: 1,
        }),
      ),
    );
    const client = new AgentClient('https://agents.example/proxy/', 'session-only-secret', fetcher);
    await client.startTurn('a/b', 'inspect files', 'retry-stable-key');
    const [url, options] = fetcher.mock.calls[0];
    expect(url).toBe('https://agents.example/proxy/v1/threads/a%2Fb/turns');
    expect(new Headers(options?.headers).get('Authorization')).toBe('Bearer session-only-secret');
    expect(options?.credentials).toBe('omit');
    expect(JSON.parse(options?.body as string)).toEqual({
      input: 'inspect files',
      idempotency_key: 'retry-stable-key',
    });
    expect(String(url)).not.toContain('secret');
    expect(options?.body).not.toContain('secret');
  });
  it('uses pagination cursor and preserves structured server failures', async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValue(
        new Response(
          JSON.stringify({ error: { code: -32009, message: 'A turn is already running' } }),
          { status: 409 },
        ),
      );
    const client = new AgentClient('http://localhost:8080', 'token', fetcher);
    await expect(client.thread('one', undefined, 'older cursor')).rejects.toMatchObject({
      status: 409,
      message: 'A turn is already running',
    });
    expect(fetcher.mock.calls[0][0]).toBe(
      'http://localhost:8080/v1/threads/one?limit=20&before=older+cursor',
    );
  });
  it('refuses credentials, query tokens and non-HTTP base URLs', () => {
    for (const url of [
      'file:///tmp/foo',
      'https://a:b@example.org',
      'https://example.org?token=x',
      'https://example.org/#x',
    ])
      expect(() => normalizeBaseUrl(url)).toThrow();
    expect(normalizeBaseUrl(' https://example.org/api/// ')).toBe('https://example.org/api');
  });
  it('reconnects with the committed cursor, ignores replayed events and stops after abort', async () => {
    const stop = new AbortController();
    const seen: number[] = [];
    const onError = vi.fn();
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(stream([frame(1).slice(0, 20), frame(1).slice(20), frame(1)]))
      .mockResolvedValueOnce(stream([frame(1), frame(2), frame(3)]));
    const client = new AgentClient('http://localhost:8080', 'secret', fetcher);
    await client.events('thread 1', {
      signal: stop.signal,
      onState: () => {},
      onError,
      reconnectDelayMs: 1,
      onEvent: (e) => {
        seen.push(e.sequence);
        if (e.sequence === 2) stop.abort();
      },
    });
    expect(seen).toEqual([1, 2]);
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(onError).not.toHaveBeenCalled();
    expect(fetcher.mock.calls[1][0]).toBe(
      'http://localhost:8080/v1/threads/thread%201/events?after=1',
    );
    expect(new Headers(fetcher.mock.calls[1][1]?.headers).get('Last-Event-ID')).toBe('1');
    expect(new Headers(fetcher.mock.calls[1][1]?.headers).get('Authorization')).toBe(
      'Bearer secret',
    );
  });
  it('decodes UTF-8 split inside a codepoint', async () => {
    const bytes = new TextEncoder().encode(
      `id: 1\nevent: agent_event\ndata: ${JSON.stringify({ ...event(1), data: { text: '中文' } })}\n\n`,
    );
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(
        new ReadableStream({
          start(controller) {
            for (const byte of bytes) controller.enqueue(new Uint8Array([byte]));
            controller.close();
          },
        }),
        { headers: { 'Content-Type': 'text/event-stream' } },
      ),
    );
    const stop = new AbortController();
    const receive = vi.fn((_event: AgentEvent) => stop.abort());
    await new AgentClient('http://localhost', '', fetcher).events('thread 1', {
      signal: stop.signal,
      onEvent: receive,
      onState: () => {},
      onError: () => {},
    });
    expect(receive.mock.calls[0][0].data).toEqual({ text: '中文' });
  });
  it('stops on an expired token or wrong-thread event, without an infinite retry', async () => {
    for (const response of [
      new Response('{"error":{"message":"Unauthorized"}}', { status: 401 }),
      stream([frame(1, 'other-thread')]),
    ]) {
      const onError = vi.fn();
      const onState = vi.fn();
      const fetcher = vi.fn<typeof fetch>().mockResolvedValue(response);
      await new AgentClient('http://localhost', 'x', fetcher).events('thread 1', {
        signal: new AbortController().signal,
        onEvent: vi.fn(),
        onState,
        onError,
        reconnectDelayMs: 1,
      });
      expect(fetcher).toHaveBeenCalledTimes(1);
      expect(onError).toHaveBeenCalledWith(expect.any(Error), true);
      expect(onState).toHaveBeenLastCalledWith('disconnected');
    }
  });
  it('aborts pending reconnect delay immediately', async () => {
    const stop = new AbortController();
    const fetcher = vi.fn<typeof fetch>().mockRejectedValue(new TypeError('offline'));
    await new AgentClient('http://localhost', '', fetcher).events('x', {
      signal: stop.signal,
      reconnectDelayMs: 60_000,
      onEvent: vi.fn(),
      onError: () => stop.abort(),
      onState: vi.fn(),
    });
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it('reconnects after a structured transient server_error from the committed cursor', async () => {
    const stop = new AbortController();
    const seen: number[] = [];
    const onError = vi.fn();
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        stream([
          frame(1),
          'event: server_error\ndata: {"error":{"code":-32010,"message":"temporarily unavailable"}}\n\n',
        ]),
      )
      .mockResolvedValueOnce(stream([frame(2)]));
    await new AgentClient('http://localhost', '', fetcher).events('thread 1', {
      signal: stop.signal,
      onState: vi.fn(),
      onError,
      reconnectDelayMs: 1,
      onEvent: (event) => {
        seen.push(event.sequence);
        if (event.sequence === 2) stop.abort();
      },
    });
    expect(seen).toEqual([1, 2]);
    expect(onError).toHaveBeenCalledWith(
      expect.objectContaining({ status: 503, message: 'temporarily unavailable' }),
      false,
    );
    expect(fetcher.mock.calls[1][0]).toContain('after=1');
  });

  it('rejects malformed timestamps, event IDs and required fields without replay loops', async () => {
    for (const data of [
      frame(1).replace('id: 1', 'id: 2'),
      frame(1).replace('1700000000000', '9007199254740991'),
      frame(1).replace('"turn_id":null,', ''),
      frame(1).replace('"data":{},', ''),
      `event: agent_event\ndata: ${'x'.repeat(2 * 1024 * 1024)}\n\n`,
    ]) {
      const fetcher = vi.fn<typeof fetch>().mockResolvedValue(stream([data]));
      const onError = vi.fn();
      await new AgentClient('http://localhost', '', fetcher).events('thread 1', {
        signal: new AbortController().signal,
        onState: vi.fn(),
        onEvent: vi.fn(),
        onError,
      });
      expect(fetcher).toHaveBeenCalledTimes(1);
      expect(onError).toHaveBeenCalledWith(expect.any(ProtocolError), true);
    }
  });

  it('validates REST shapes, status values and thread ownership before returning state', async () => {
    const base = {
      thread: {
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
      },
      turns: [
        {
          id: 'turn',
          thread_id: 'one',
          input: 'hi',
          status: 'completed',
          output: 'done',
          error: null,
          created_at: 1,
          updated_at: 1,
        },
      ],
      next_cursor: null,
    };
    for (const value of [
      {},
      { ...base, thread: { ...base.thread, id: 'other' } },
      { ...base, turns: [{ ...base.turns[0], thread_id: 'other' }] },
      { ...base, turns: [{ ...base.turns[0], status: 'unknown' }] },
      { ...base, turns: [{ ...base.turns[0], created_at: 9e15 }] },
    ]) {
      const fetcher = vi.fn<typeof fetch>().mockResolvedValue(new Response(JSON.stringify(value)));
      await expect(
        new AgentClient('http://localhost', '', fetcher).thread('one'),
      ).rejects.toBeInstanceOf(ProtocolError);
    }
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(new Response(JSON.stringify(base)));
    await expect(new AgentClient('http://localhost', '', fetcher).thread('one')).resolves.toEqual(
      base,
    );
  });
});

it('merges paginated snapshots without regressing a completed turn', () => {
  const base: Turn = {
    id: 't2',
    thread_id: 'one',
    input: 'hi',
    output: 'done',
    error: null,
    status: 'completed',
    created_at: 2,
    updated_at: 10,
  };
  const result = mergeTurns(
    [base],
    [
      { ...base, status: 'running', output: null, updated_at: 10 },
      { ...base, id: 't1', created_at: 1 },
    ],
  );
  expect(result.map((t) => t.id)).toEqual(['t1', 't2']);
  expect(result[1].status).toBe('completed');
  expect(result[1].output).toBe('done');
});
