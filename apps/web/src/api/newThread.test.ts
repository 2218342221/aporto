import { describe, expect, it, vi } from 'vitest';
import { AgentClient } from './client';
import { mergeInstances, normalizeWorkdir, threadRequest } from './newThread';
import type { ThreadChoices } from './newThread';
import type { AgentSummary, RuntimeInstance } from './types';
import { agentList, instanceList, ProtocolError } from './validation';

const agent: AgentSummary = {
  id: 'reviewer',
  name: 'Reviewer',
  model: 'model',
  release_id: 'current',
  bundle_digest: 'current-bundle',
  runtime: {
    provider: 'docker',
    images: ['node:22', 'python:3.12'],
    default_image: 'node:22',
    default_workdir: '/workspace',
  },
};
const instance: RuntimeInstance = {
  id: 'instance',
  agent_id: agent.id,
  release_id: 'old',
  bundle_digest: 'old-bundle',
  provider: 'docker',
  image: 'node:20',
  sandbox_id: 'sandbox',
  workdir: '/workspace/legacy',
  busy: true,
  created_at: 1,
  updated_at: 2,
};
const choices: ThreadChoices = {
  title: 'Title',
  mode: 'new',
  image: 'node:22',
  instanceId: '',
  workdir: '/workspace',
};

describe('new conversation configuration', () => {
  it('submits explicit new-image selection and normalized custom directory', () => {
    expect(
      threadRequest(agent, { ...choices, image: 'python:3.12', workdir: '/srv/project/' }),
    ).toEqual({
      agent_id: agent.id,
      title: 'Title',
      runtime: { mode: 'new', image: 'python:3.12' },
      workdir: '/srv/project',
    });
    expect(() => threadRequest(agent, { ...choices, image: 'foreign' })).toThrow('可用镜像');
  });
  it('allows a busy instance with an old immutable release and image outside the current list', () => {
    const request = threadRequest(
      agent,
      { ...choices, mode: 'reuse', instanceId: instance.id, workdir: '/data/task' },
      instance,
    );
    expect(request.runtime).toEqual({ mode: 'reuse', instance_id: instance.id });
    expect(request.workdir).toBe('/data/task');
    expect(request.runtime).not.toHaveProperty('image');
    expect(() =>
      threadRequest(
        agent,
        { ...choices, mode: 'reuse', instanceId: instance.id },
        { ...instance, agent_id: 'other' },
      ),
    ).toThrow('此 Agent');
    expect(() => threadRequest(agent, { ...choices, mode: 'reuse' })).toThrow('此 Agent');
  });
  it('allows non-workspace directories but rejects unsafe or reserved paths and oversized UTF-8', () => {
    expect(normalizeWorkdir('/data/jobs/')).toBe('/data/jobs');
    expect(normalizeWorkdir('/workspace/中文目录')).toBe('/workspace/中文目录');
    for (const value of [
      '/',
      'relative',
      '/workspace//x',
      '/workspace/../x',
      '/workspace/./x',
      '/workspace\\x',
      '/workspace/\u0001x',
      '/workspace\n',
      '/opt/agent',
      '/opt/agent/skills',
      '/tmp/.aporto-session',
      '/.aporto/state',
      '/data/' + '字'.repeat(1400),
    ])
      expect(() => normalizeWorkdir(value)).toThrow();
  });
  it('retains newer instance metadata when a paginated response overlaps', () => {
    const latest = { ...instance, busy: false, updated_at: 4 };
    expect(mergeInstances([latest], [instance])).toEqual([latest]);
  });
});

describe('runtime configuration wire contract', () => {
  it('requires coherent agent defaults while accepting old-release instances for the same agent', () => {
    expect(agentList({ agents: [agent] }).agents).toEqual([agent]);
    for (const runtime of [
      undefined,
      { ...agent.runtime, images: [] },
      { ...agent.runtime, default_image: 'foreign' },
      { ...agent.runtime, images: ['node:22', 'node:22'] },
    ])
      expect(() => agentList({ agents: [{ ...agent, runtime }] })).toThrow(ProtocolError);
    expect(instanceList(agent.id)({ instances: [instance], next_cursor: null }).instances).toEqual([
      instance,
    ]);
    for (const bad of [
      { ...instance, agent_id: 'other' },
      { ...instance, busy: 'yes' },
      { ...instance, sandbox_id: null },
    ])
      expect(() => instanceList(agent.id)({ instances: [bad], next_cursor: null })).toThrow(
        ProtocolError,
      );
    expect(() =>
      instanceList(agent.id, 'cursor')({ instances: [], next_cursor: 'cursor' }),
    ).toThrow(ProtocolError);
  });
  it('encodes instance list paths/cursors and sends the selected creation body without extra image fields', async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            instances: [{ ...instance, agent_id: 'agent/one' }],
            next_cursor: null,
          }),
        ),
      )
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            id: 'thread',
            title: 'Title',
            agent_id: agent.id,
            release_id: instance.release_id,
            bundle_digest: instance.bundle_digest,
            sandbox_id: instance.sandbox_id,
            runtime_instance_id: instance.id,
            runtime_image: instance.image,
            runtime_provider: instance.provider,
            workdir: '/data/task',
            created_at: 1,
            updated_at: 1,
            last_sequence: 0,
          }),
        ),
      );
    const client = new AgentClient('http://localhost', 'token', fetcher);
    await client.instances('agent/one', undefined, 'next cursor');
    expect(fetcher.mock.calls[0][0]).toBe(
      'http://localhost/v1/agents/agent%2Fone/instances?limit=50&cursor=next+cursor',
    );
    await client.createThread({
      agent_id: agent.id,
      runtime: { mode: 'reuse', instance_id: instance.id },
      workdir: '/data/task',
    });
    expect(JSON.parse(fetcher.mock.calls[1][1]?.body as string)).toEqual({
      agent_id: agent.id,
      runtime: { mode: 'reuse', instance_id: instance.id },
      workdir: '/data/task',
      title: null,
    });
  });
});
