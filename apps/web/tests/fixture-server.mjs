// Deterministic browser fixtures. No model, MCP, or AgentENV is called here.
import http from 'node:http';

const sessions = new Map();
const now = () => Date.now();
const agents = [
  {
    id: 'reviewer',
    name: '代码审阅 Agent',
    model: 'server-managed',
    bundle_digest: 'sha256:fixture-reviewer',
    release_id: 'sha256:release-reviewer',
    runtime: {
      provider: 'docker',
      images: ['node:22-bookworm', 'python:3.12-slim'],
      default_image: 'node:22-bookworm',
      default_workdir: '/workspace',
    },
  },
  {
    id: 'researcher',
    name: '研究助理',
    model: 'server-managed',
    bundle_digest: 'sha256:fixture-researcher',
    release_id: 'sha256:release-researcher',
    runtime: {
      provider: 'agentenv',
      images: ['research-v1', 'research-v2'],
      default_image: 'research-v1',
      default_workdir: '/workspace/research',
    },
  },
];
function seed() {
  const time = now();
  const threads = [
    {
      id: 'architecture',
      title: '梳理项目结构与关键模块',
      agent_id: 'reviewer',
      bundle_digest: agents[0].bundle_digest,
      release_id: agents[0].release_id,
      sandbox_id: 'fixture-sandbox',
      runtime_instance_id: 'instance-reviewer-default',
      runtime_provider: 'docker',
      runtime_image: 'node:22-bookworm',
      workdir: '/workspace',
      created_at: time - 3600000,
      updated_at: time - 60000,
      last_sequence: 7,
    },
    {
      id: 'research',
      title: '整理相关工作与研究方向',
      agent_id: 'researcher',
      bundle_digest: agents[1].bundle_digest,
      release_id: agents[1].release_id,
      sandbox_id: null,
      runtime_instance_id: 'instance-researcher-new',
      runtime_provider: 'agentenv',
      runtime_image: 'research-v1',
      workdir: '/workspace/research',
      created_at: time - 8000000,
      updated_at: time - 7200000,
      last_sequence: 1,
    },
    {
      id: 'docs',
      title: '完善项目的使用文档',
      agent_id: 'reviewer',
      bundle_digest: agents[0].bundle_digest,
      release_id: agents[0].release_id,
      sandbox_id: null,
      runtime_instance_id: 'instance-docs-new',
      runtime_provider: 'docker',
      runtime_image: 'node:22-bookworm',
      workdir: '/workspace',
      created_at: time - 90000000,
      updated_at: time - 86400000,
      last_sequence: 1,
    },
  ];
  const turns = [
    {
      id: 'initial',
      thread_id: 'architecture',
      input: '请梳理这个项目的结构，说明各个模块的职责，以及一次任务从发起到完成的执行流程。',
      status: 'completed',
      output:
        '已完成项目结构梳理。当前项目围绕 **构建、执行、交互** 三个环节组织。\n\n### 关键模块\n\n| 模块 | 主要职责 |\n| --- | --- |\n| Agentfile 构建 | 打包提示词、技能与 MCP 配置 |\n| Agent Core | 管理会话、调度任务与执行工具 |\n| Runtime | 提供隔离的任务运行环境 |\n| Web 工作台 | 发起对话并查看实时进度 |\n\n### 一次任务如何完成\n\n1. 用户选择 Agent 并发送目标。\n2. Core 准备运行环境，加载已打包的配置。\n3. Agent 调用工具推进任务，执行过程持续上报。\n4. 结果回到会话，完整执行记录可继续查阅。\n\n你可以继续指定一个模块，我会进一步查看它的实现。',
      error: null,
      created_at: time - 120000,
      updated_at: time - 60000,
    },
  ];
  const kinds = [
    'thread.started',
    'turn.queued',
    'turn.started',
    'runtime.ready',
    'model.started',
    'tool.completed',
    'turn.completed',
  ];
  const events = kinds.map((kind, i) => ({
    sequence: i + 1,
    thread_id: 'architecture',
    turn_id: i ? 'initial' : null,
    kind,
    data: {},
    created_at: time - 125000 + i * 10000,
  }));
  const instances = [
    {
      id: 'instance-reviewer-default',
      agent_id: 'reviewer',
      release_id: agents[0].release_id,
      bundle_digest: agents[0].bundle_digest,
      provider: 'docker',
      image: 'node:22-bookworm',
      sandbox_id: 'fixture-sandbox',
      workdir: '/workspace',
      busy: false,
      created_at: time - 3600000,
      updated_at: time - 60000,
    },
    {
      id: 'instance-reviewer-legacy',
      agent_id: 'reviewer',
      release_id: 'sha256:release-reviewer-old',
      bundle_digest: 'sha256:bundle-reviewer-old',
      provider: 'docker',
      image: 'node:20-bookworm',
      sandbox_id: 'legacy-sandbox',
      workdir: '/workspace/legacy',
      busy: true,
      created_at: time - 90000000,
      updated_at: time - 10000,
    },
    {
      id: 'instance-researcher-ready',
      agent_id: 'researcher',
      release_id: agents[1].release_id,
      bundle_digest: agents[1].bundle_digest,
      provider: 'agentenv',
      image: 'research-v1',
      sandbox_id: 'research-sandbox',
      workdir: '/workspace/research',
      busy: false,
      created_at: time - 70000,
      updated_at: time - 60000,
    },
  ];
  return {
    threads,
    turns,
    instances,
    items: [],
    events,
    clients: new Set(),
    history: [],
    nextId: 1,
  };
}
const json = (response, code, value) => {
  response.writeHead(code, { 'Content-Type': 'application/json' });
  response.end(JSON.stringify(value));
};
const frame = (event) =>
  `id: ${event.sequence}\nevent: agent_event\ndata: ${JSON.stringify(event)}\n\n`;
function emit(session, thread, kind, turnId, data = {}) {
  const event = {
    sequence: ++thread.last_sequence,
    thread_id: thread.id,
    turn_id: turnId ?? null,
    kind,
    data,
    created_at: now(),
  };
  session.events.push(event);
  thread.updated_at = now();
  for (const client of session.clients)
    if (client.id === thread.id) client.response.write(frame(event));
  return event;
}
function putItem(session, turn, patch, silent = false) {
  const thread = session.threads.find((value) => value.id === turn.thread_id);
  const previous = session.items.find(
    (value) => value.id === patch.id && value.turn_id === turn.id,
  );
  const sequence = thread.last_sequence + 1;
  const item = {
    id: patch.id,
    turn_id: turn.id,
    ordinal: sequence,
    kind: 'assistant_message',
    status: 'in_progress',
    phase: 'commentary',
    name: null,
    text: null,
    input: null,
    output: null,
    error: null,
    elapsed_ms: null,
    truncated: false,
    created_at: now(),
    ...previous,
    ...patch,
    sequence,
    updated_at: now(),
  };
  if (previous) session.items.splice(session.items.indexOf(previous), 1, item);
  else session.items.push(item);
  const kind =
    item.status === 'in_progress' ? (previous ? 'item.updated' : 'item.started') : 'item.completed';
  if (silent) thread.last_sequence = sequence;
  else emit(session, thread, kind, turn.id, { item });
  return item;
}
function finishItems(session, turn, status) {
  for (const item of session.items.filter(
    (value) => value.turn_id === turn.id && value.status === 'in_progress',
  ))
    putItem(session, turn, {
      ...item,
      status,
      updated_at: now(),
      elapsed_ms: now() - item.created_at,
    });
}
async function body(request) {
  let value = '';
  for await (const chunk of request) value += chunk;
  return value ? JSON.parse(value) : {};
}

const server = http.createServer(async (request, response) => {
  response.setHeader('Access-Control-Allow-Origin', '*');
  response.setHeader('Access-Control-Allow-Headers', 'Authorization, Content-Type, Last-Event-ID');
  response.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS');
  if (request.method === 'OPTIONS') {
    response.writeHead(204);
    response.end();
    return;
  }
  const url = new URL(request.url, 'http://127.0.0.1:4318');
  if (url.pathname === '/health') return json(response, 200, { ok: true });
  const token = request.headers.authorization?.replace(/^Bearer /, '') ?? '';
  if (!token || token === 'invalid-token')
    return json(response, 401, { error: { code: 401, message: '访问令牌无效' } });
  if (!sessions.has(token)) sessions.set(token, seed());
  const session = sessions.get(token);
  session.history.push({
    path: url.pathname,
    after: url.searchParams.get('after'),
    last: request.headers['last-event-id'],
    method: request.method,
  });
  if (url.pathname === '/_fixture') {
    if (request.method === 'GET')
      return json(response, 200, { history: session.history, items: session.items });
    const data = await body(request);
    if (data.action === 'disconnect') for (const client of session.clients) client.response.end();
    if (data.action === 'stream-error') {
      for (const client of session.clients) {
        client.response.write(
          'event: server_error\ndata: {"error":{"code":-32010,"message":"temporarily unavailable"}}\n\n',
        );
        client.response.end();
      }
    }
    if (data.action === 'event') {
      emit(session, session.threads[0], 'tool.completed', null);
    }
    if (data.action === 'append-instances') {
      for (let index = 0; index < data.count; index++)
        session.instances.push({
          ...session.instances[0],
          id: `extra-instance-${index}`,
          sandbox_id: `extra-sandbox-${index}`,
          workdir: `/workspace/project-${index}`,
        });
    }
    if (data.action === 'item') {
      const turn = data.turn_id
        ? session.turns.find((value) => value.id === data.turn_id)
        : session.turns.findLast((value) => value.status === 'running');
      if (!turn) return json(response, 404, { error: { message: 'Unknown turn' } });
      putItem(session, turn, data.item, data.silent === true);
    }
    if (data.action === 'append-items') {
      const turn = session.turns.find((value) => value.id === (data.turn_id ?? 'initial'));
      for (let index = 0; index < data.count; index++)
        putItem(
          session,
          turn,
          { id: `history-item-${index}`, text: `历史活动 ${index + 1}`, status: 'completed' },
          data.silent === true,
        );
    }
    if (data.action === 'append-turns') {
      const thread = session.threads[0];
      for (let i = 0; i < data.count; i++) {
        session.turns.push({
          id: `appended-${session.nextId++}`,
          thread_id: thread.id,
          input: `补齐第${i + 1}条消息`,
          output: '已完成',
          status: 'completed',
          error: null,
          created_at: now() + i,
          updated_at: now() + i,
        });
      }
      emit(session, thread, 'turn.completed', session.turns.at(-1).id);
    }
    if (data.action === 'complete' || data.action === 'fail') {
      const turn = session.turns.findLast((t) => t.status === 'running');
      if (turn) {
        turn.status = data.action === 'fail' ? 'failed' : 'completed';
        const final = session.items.findLast(
          (item) => item.turn_id === turn.id && item.phase === 'final_answer',
        );
        turn.output =
          data.output ?? final?.text ?? (data.action === 'fail' ? null : '任务已完成：验证通过。');
        turn.error = data.action === 'fail' ? '模型连接失败' : null;
        turn.updated_at = now();
        finishItems(session, turn, turn.status);
        emit(
          session,
          session.threads.find((t) => t.id === turn.thread_id),
          `turn.${turn.status}`,
          turn.id,
        );
      }
    }
    if (data.action === 'duplicate') {
      const event = session.events.at(-1);
      for (const client of session.clients)
        if (client.id === event.thread_id) client.response.write(frame(event));
    }
    return json(response, 200, { ok: true });
  }
  if (url.pathname === '/v1/agents') return json(response, 200, { agents });
  const instanceRoute = /^\/v1\/agents\/([^/]+)\/instances$/.exec(url.pathname);
  if (instanceRoute && request.method === 'GET') {
    const agentId = decodeURIComponent(instanceRoute[1]);
    const instances = session.instances.filter((instance) => instance.agent_id === agentId);
    const cursor = url.searchParams.get('cursor');
    const start = cursor ? instances.findIndex((instance) => instance.id === cursor) + 1 : 0;
    const page = instances.slice(start, start + Number(url.searchParams.get('limit') ?? 50));
    return json(response, 200, {
      instances: page,
      next_cursor: start + page.length < instances.length ? page.at(-1).id : null,
    });
  }
  if (url.pathname === '/v1/threads') {
    if (request.method === 'GET')
      return json(response, 200, { threads: session.threads, next_cursor: null });
    const data = await body(request);
    const agent = agents.find((a) => a.id === data.agent_id);
    if (!agent) return json(response, 400, { error: { message: 'Agent does not exist' } });
    const reuse = data.runtime?.mode === 'reuse';
    const instance = reuse
      ? session.instances.find(
          (value) => value.id === data.runtime.instance_id && value.agent_id === agent.id,
        )
      : undefined;
    if (reuse && !instance)
      return json(response, 404, { error: { message: 'Instance does not exist' } });
    const image = instance?.image ?? data.runtime?.image ?? agent.runtime.default_image;
    if (!reuse && !agent.runtime.images.includes(image))
      return json(response, 400, { error: { message: 'Image is not allowed' } });
    const thread = {
      id: `created-${session.nextId++}`,
      title: data.title || '新会话',
      agent_id: agent.id,
      bundle_digest: instance?.bundle_digest ?? agent.bundle_digest,
      release_id: instance?.release_id ?? agent.release_id,
      sandbox_id: instance?.sandbox_id ?? null,
      runtime_instance_id: instance?.id ?? `new-instance-${session.nextId++}`,
      runtime_provider: instance?.provider ?? agent.runtime.provider,
      runtime_image: image,
      workdir: data.workdir ?? instance?.workdir ?? agent.runtime.default_workdir,
      created_at: now(),
      updated_at: now(),
      last_sequence: 0,
    };
    session.threads.unshift(thread);
    emit(session, thread, 'thread.started');
    return json(response, 201, thread);
  }
  const match = /^\/v1\/threads\/([^/]+)(.*)$/.exec(url.pathname);
  if (!match) return json(response, 404, { error: { message: 'Not found' } });
  const id = decodeURIComponent(match[1]);
  const tail = match[2];
  const thread = session.threads.find((t) => t.id === id);
  if (!thread) return json(response, 404, { error: { message: 'Unknown thread' } });
  if (!tail) {
    const turns = session.turns.filter((t) => t.thread_id === id);
    const before = turns.findIndex((turn) => turn.id === url.searchParams.get('before'));
    const end = before < 0 ? turns.length : before;
    const start = Math.max(0, end - Number(url.searchParams.get('limit') ?? 20));
    return json(response, 200, {
      thread,
      turns: turns.slice(start, end),
      next_cursor: start ? turns[start].id : null,
    });
  }
  if (tail === '/events') {
    response.writeHead(200, {
      'Content-Type': 'text/event-stream',
      'Cache-Control': 'no-cache',
      Connection: 'keep-alive',
    });
    response.write(': connected\n\n');
    const after = Number(url.searchParams.get('after') ?? request.headers['last-event-id'] ?? 0);
    // One duplicate verifies that client resume tolerates replay at the boundary.
    for (const event of session.events)
      if (event.thread_id === id && event.sequence >= after) response.write(frame(event));
    const client = { id, response };
    session.clients.add(client);
    const keepalive = setInterval(() => response.write(': heartbeat\n\n'), 15000);
    response.on('close', () => {
      clearInterval(keepalive);
      session.clients.delete(client);
    });
    return;
  }
  if (tail === '/turns' && request.method === 'POST') {
    const data = await body(request);
    const existing = session.turns.find(
      (t) => t.key === data.idempotency_key && t.thread_id === id,
    );
    if (existing) return json(response, 202, existing);
    if (
      session.turns.some(
        (t) => t.thread_id === id && ['queued', 'running', 'cancelling'].includes(t.status),
      )
    )
      return json(response, 409, { error: { message: '已有任务正在执行' } });
    const turn = {
      id: `turn-${session.nextId++}`,
      thread_id: id,
      input: data.input,
      status: 'running',
      output: null,
      error: null,
      created_at: now(),
      updated_at: now(),
      key: data.idempotency_key,
    };
    session.turns.push(turn);
    let instance = session.instances.find((value) => value.id === thread.runtime_instance_id);
    if (!instance) {
      instance = {
        id: thread.runtime_instance_id,
        agent_id: thread.agent_id,
        release_id: thread.release_id,
        bundle_digest: thread.bundle_digest,
        provider: thread.runtime_provider,
        image: thread.runtime_image,
        sandbox_id: `sandbox-${thread.runtime_instance_id}`,
        workdir: thread.workdir,
        busy: true,
        created_at: now(),
        updated_at: now(),
      };
      session.instances.push(instance);
    }
    thread.sandbox_id = instance.sandbox_id;
    emit(session, thread, 'turn.queued', turn.id);
    emit(session, thread, 'turn.started', turn.id);
    emit(session, thread, 'runtime.ready', turn.id);
    emit(session, thread, 'model.started', turn.id);
    return json(response, 202, turn);
  }
  const itemsMatch = /^\/turns\/([^/]+)\/items$/.exec(tail);
  if (itemsMatch && request.method === 'GET') {
    const turnId = decodeURIComponent(itemsMatch[1]);
    const after = Number(url.searchParams.get('after') ?? 0);
    const all = session.items
      .filter((item) => item.turn_id === turnId && item.ordinal > after)
      .sort((a, b) => a.ordinal - b.ordinal);
    const items = all.slice(0, Number(url.searchParams.get('limit') ?? 100));
    return json(response, 200, {
      items,
      next_cursor: items.at(-1)?.ordinal ?? after,
      has_more: all.length > items.length,
    });
  }
  const interrupt = /^\/turns\/([^/]+)\/interrupt$/.exec(tail);
  if (interrupt) {
    const turn = session.turns.find((t) => t.id === interrupt[1] && t.thread_id === id);
    if (!turn) return json(response, 404, { error: { message: 'Unknown turn' } });
    turn.status = 'interrupted';
    turn.updated_at = now();
    finishItems(session, turn, 'interrupted');
    emit(session, thread, 'turn.interrupted', turn.id);
    return json(response, 200, turn);
  }
  return json(response, 404, { error: { message: 'Not found' } });
});
server.listen(4318, '127.0.0.1');
process.on('SIGTERM', () => {
  for (const session of sessions.values())
    for (const client of session.clients) client.response.end();
  server.close();
});
