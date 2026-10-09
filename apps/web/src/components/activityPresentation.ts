import type { TurnItem } from '../api/types';

type ObjectValue = Record<string, unknown>;

export function objectPayload(value: string | null): ObjectValue | null {
  if (value === null) return null;
  try {
    const parsed: unknown = JSON.parse(value);
    return parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)
      ? (parsed as ObjectValue)
      : null;
  } catch {
    return null;
  }
}

function field(value: ObjectValue | null, name: string): string {
  return typeof value?.[name] === 'string' ? value[name] : '';
}

export function toolName(item: TurnItem): string {
  return (item.name || (item.kind === 'ptc_call' ? 'exec' : '工具')).replace(/^tools\./, '');
}

export function operation(item: TurnItem) {
  const name = toolName(item);
  const input = objectPayload(item.input);
  const output = objectPayload(item.output);
  switch (name) {
    case 'exec_command':
      return {
        icon: 'terminal',
        verb: '运行',
        subject: field(input, 'command') || field(input, 'cmd') || '命令',
        category: 'command',
      } as const;
    case 'read_file':
      return {
        icon: 'file',
        verb: '读取',
        subject: field(output, 'path') || field(input, 'path') || '文件',
        category: 'read',
      } as const;
    case 'write_file':
      return {
        icon: 'edit',
        verb: '写入',
        subject: field(output, 'path') || field(input, 'path') || '文件',
        category: 'write',
      } as const;
    case 'read_skill':
      return {
        icon: 'file',
        verb: '读取',
        subject: field(input, 'name') || '技能',
        category: 'skill',
      } as const;
    case 'tool_search':
      return {
        icon: 'search',
        verb: '搜索',
        subject: field(input, 'query') || '工具',
        category: 'search',
      } as const;
    default:
      return item.kind === 'ptc_call'
        ? ({
            icon: 'code',
            verb: '执行',
            subject: name === 'wait' ? '等待结果' : '程序',
            category: 'program',
          } as const)
        : ({ icon: 'bolt', verb: '调用', subject: name, category: 'tool' } as const);
  }
}

export function operationLabel(item: TurnItem): string {
  const { verb, subject } = operation(item);
  const prefix =
    item.status === 'in_progress'
      ? `正在${verb}`
      : item.status === 'failed'
        ? `${verb}失败`
        : item.status === 'interrupted'
          ? '已中断'
          : `已${verb}`;
  return `${prefix} ${subject.replace(/\s+/g, ' ').trim()}`;
}

export type ActivityEntry =
  | { kind: 'message'; item: TurnItem }
  | { kind: 'operation'; item: TurnItem; tools: TurnItem[] };

/**
 * The protocol exposes creation order, not parent call IDs. Group only an exec
 * and its immediately following tools; messages and further PTC calls are hard
 * boundaries. Every original record remains reachable in its original order.
 */
export function activityEntries(items: TurnItem[]): ActivityEntry[] {
  const entries: ActivityEntry[] = [];
  let group: Extract<ActivityEntry, { kind: 'operation' }> | null = null;
  for (const item of items) {
    if (item.kind === 'assistant_message') {
      group = null;
      if (item.phase !== 'final_answer') entries.push({ kind: 'message', item });
    } else if (item.kind === 'tool_call' && group) {
      group.tools.push(item);
    } else {
      const entry: Extract<ActivityEntry, { kind: 'operation' }> = {
        kind: 'operation',
        item,
        tools: [],
      };
      entries.push(entry);
      group = item.kind === 'ptc_call' ? entry : null;
    }
  }
  return entries;
}

export function groupLabel(item: TurnItem, tools: TurnItem[]): string {
  if (tools.length === 1) {
    if (item.status === 'failed' || item.status === 'interrupted') {
      return `${operationLabel(tools[0])} · ${item.status === 'failed' ? '程序失败' : '程序已中断'}`;
    }
    return operationLabel(tools[0]);
  }
  const count = new Map<string, number>();
  for (const tool of tools) {
    const category = operation(tool).category;
    count.set(category, (count.get(category) ?? 0) + 1);
  }
  const phrases: Record<string, (n: number) => string> = {
    command: (n) => `运行 ${n} 条命令`,
    read: (n) => `读取文件 ${n} 次`,
    write: (n) => `写入文件 ${n} 次`,
    skill: (n) => `读取 ${n} 项技能`,
    search: (n) => `搜索 ${n} 次`,
    tool: (n) => `调用 ${n} 个工具`,
  };
  const text = [...count].map(([category, n]) => (phrases[category] ?? phrases.tool)(n)).join('，');
  const failed = item.status === 'failed' || tools.some((tool) => tool.status === 'failed');
  const interrupted =
    item.status === 'interrupted' || tools.some((tool) => tool.status === 'interrupted');
  const running =
    item.status === 'in_progress' || tools.some((tool) => tool.status === 'in_progress');
  return `${running ? '正在' : '已'}${text}${failed ? ' · 执行失败' : interrupted ? ' · 已中断' : ''}`;
}

export interface WrittenFile {
  itemId: string;
  path: string;
  bytes: number;
  content: string;
  truncated: boolean;
}

export function writtenFiles(items: TurnItem[]): WrittenFile[] {
  const files = new Map<string, WrittenFile>();
  for (const item of items) {
    if (item.kind !== 'tool_call' || toolName(item) !== 'write_file' || item.status !== 'completed')
      continue;
    const input = objectPayload(item.input);
    const output = objectPayload(item.output);
    if (
      !output ||
      typeof output.path !== 'string' ||
      !output.path ||
      typeof output.bytes !== 'number' ||
      !Number.isSafeInteger(output.bytes) ||
      output.bytes < 0
    )
      continue;
    if (!input || typeof input.content !== 'string') continue;
    // A card is a receipt of this successful tool call, never a filesystem diff.
    files.set(output.path, {
      itemId: item.id,
      path: output.path,
      bytes: output.bytes,
      content: input.content,
      truncated: item.truncated,
    });
  }
  return [...files.values()];
}
