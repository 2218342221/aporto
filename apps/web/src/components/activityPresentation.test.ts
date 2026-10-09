import { describe, expect, it } from 'vitest';
import type { TurnItem } from '../api/types';
import { activityEntries, groupLabel, operationLabel, writtenFiles } from './activityPresentation';

function item(id: string, patch: Partial<TurnItem> = {}): TurnItem {
  return {
    id,
    turn_id: 'turn',
    ordinal: 1,
    sequence: 1,
    kind: 'tool_call',
    status: 'completed',
    phase: null,
    name: 'tools.exec_command',
    text: null,
    input: '{"cmd":"cargo test"}',
    output: null,
    error: null,
    elapsed_ms: 1,
    truncated: false,
    created_at: 1,
    updated_at: 2,
    ...patch,
  };
}

function write(id: string, content: string, patch: Partial<TurnItem> = {}): TurnItem {
  return item(id, {
    name: 'tools.write_file',
    input: JSON.stringify({ path: 'main.rs', content }),
    output: JSON.stringify({
      path: '/workspace/main.rs',
      bytes: new TextEncoder().encode(content).length,
    }),
    ...patch,
  });
}

describe('conversation activity presentation', () => {
  it('keeps source and tool records in order without grouping across messages or another PTC call', () => {
    const entries = activityEntries([
      item('exec-a', { kind: 'ptc_call', name: 'exec', output: 'source output' }),
      item('tool-a'),
      item('comment', { kind: 'assistant_message', phase: 'commentary', text: 'progress' }),
      item('ungrouped'),
      item('exec-b', { kind: 'ptc_call', name: 'exec' }),
      item('tool-b'),
      item('wait', { kind: 'ptc_call', name: 'wait', output: 'wait output' }),
      item('tool-c'),
      item('final', { kind: 'assistant_message', phase: 'final_answer', text: 'answer' }),
      item('after-final'),
    ]);
    expect(
      entries.map((entry) => [
        entry.item.id,
        entry.kind === 'operation' ? entry.tools.map((tool) => tool.id) : [],
      ]),
    ).toEqual([
      ['exec-a', ['tool-a']],
      ['comment', []],
      ['ungrouped', []],
      ['exec-b', ['tool-b']],
      ['wait', ['tool-c']],
      ['after-final', []],
    ]);
    expect(entries[0].item.output).toBe('source output');
    expect(entries[4].item.output).toBe('wait output');
  });

  it('shows active children after exec completes and keeps a parent failure visible', () => {
    const completed = item('exec', { kind: 'ptc_call', name: 'exec' });
    const active = item('active', { status: 'in_progress' });
    expect(groupLabel(completed, [active])).toBe('正在运行 cargo test');
    expect(groupLabel(completed, [item('done'), active])).toMatch(/^正在/);
    expect(groupLabel({ ...completed, status: 'failed' }, [item('done')])).toContain('程序失败');
    expect(
      operationLabel(item('read', { name: 'tools.read_file', input: '{"path":"Cargo.toml"}' })),
    ).toBe('已读取 Cargo.toml');
  });

  it('retains only the latest successful write receipt for a path', () => {
    const files = writtenFiles([
      write('first', 'old'),
      write('latest', '新的内容\n'),
      write('failed', 'not written', { status: 'failed', output: null }),
    ]);
    expect(files).toEqual([
      {
        itemId: 'latest',
        path: '/workspace/main.rs',
        bytes: 13,
        content: '新的内容\n',
        truncated: false,
      },
    ]);
  });

  it('does not create file receipts from malformed, incomplete, or truncated-envelope payloads', () => {
    expect(
      writtenFiles([
        write('bad-input', 'ignored', { input: '{"content":"unfinished' }),
        write('bad-output', 'ignored', { output: '{"path":' }),
        write('truncated-output', 'ignored', {
          truncated: true,
          output: '{"truncated":true,"preview":"{\\"path\\":\\"/workspace/main.rs"}',
        }),
        write('missing-content', 'ignored', { input: '{"path":"main.rs"}' }),
        write('negative-bytes', 'ignored', { output: '{"path":"/workspace/main.rs","bytes":-1}' }),
        write('fractional-bytes', 'ignored', {
          output: '{"path":"/workspace/main.rs","bytes":0.5}',
        }),
        write('pending', 'ignored', { status: 'in_progress' }),
        write('different-tool', 'ignored', { name: 'tools.mcp__file_writer' }),
      ]),
    ).toEqual([]);
  });
});
