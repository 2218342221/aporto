import type { AgentSummary, RuntimeInstance, ThreadCreateInput } from './types';

export interface ThreadChoices {
  title: string;
  mode: 'new' | 'reuse';
  image: string;
  instanceId: string;
  workdir: string;
}

/** Match the server's container-path contract; the server remains authoritative. */
export function normalizeWorkdir(value: string): string {
  const path = value.trim();
  if (
    !path.startsWith('/') ||
    path === '/' ||
    path.includes('//') ||
    path.includes('\\') ||
    /\p{Cc}/u.test(value) ||
    new TextEncoder().encode(path).length > 4096 ||
    path.split('/').some((part) => part === '.' || part === '..')
  )
    throw new Error('请填写有效的绝对工作目录，例如 /workspace/project');
  const normalized = path.endsWith('/') ? path.slice(0, -1) : path;
  if (
    normalized === '/opt/agent' ||
    normalized.startsWith('/opt/agent/') ||
    normalized.startsWith('/tmp/.aporto') ||
    normalized === '/.aporto' ||
    normalized.startsWith('/.aporto/')
  )
    throw new Error('此目录由 Aporto 保留，请选择其他工作目录');
  return normalized;
}

export function threadRequest(
  agent: AgentSummary | undefined,
  choices: ThreadChoices,
  instance?: RuntimeInstance,
): ThreadCreateInput {
  if (!agent) throw new Error('请选择 Agent');
  const workdir = normalizeWorkdir(choices.workdir);
  let runtime: ThreadCreateInput['runtime'];
  if (choices.mode === 'new') {
    if (!agent.runtime.images.includes(choices.image)) throw new Error('请选择可用镜像');
    runtime = { mode: 'new', image: choices.image };
  } else {
    if (!instance || instance.id !== choices.instanceId || instance.agent_id !== agent.id)
      throw new Error('请选择此 Agent 的已有实例');
    // Reusing binds the instance's original release and image. Its image need not
    // appear in the active agent's image list, and a busy instance is valid.
    runtime = { mode: 'reuse', instance_id: instance.id };
  }
  return { agent_id: agent.id, title: choices.title.trim() || null, runtime, workdir };
}

export function mergeInstances(current: RuntimeInstance[], incoming: RuntimeInstance[]) {
  const map = new Map(current.map((instance) => [instance.id, instance]));
  for (const instance of incoming) {
    const previous = map.get(instance.id);
    if (!previous || instance.updated_at >= previous.updated_at) map.set(instance.id, instance);
  }
  return [...map.values()];
}
