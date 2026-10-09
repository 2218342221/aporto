import { useEffect, useState } from 'react';
import type { Turn, TurnItem } from '../api/types';
import { isActive } from '../api/types';
import type { TurnItemsState } from '../api/items';
import { copyText } from '../api/clipboard';
import { Icon } from './Icon';
import { Markdown } from './Markdown';
import {
  activityEntries,
  groupLabel,
  operation,
  operationLabel,
  toolName,
  writtenFiles,
} from './activityPresentation';
import './turn-activity.css';

export function ElapsedTime({
  active,
  startedAt,
  updatedAt,
  elapsedMs,
}: {
  active: boolean;
  startedAt: number;
  updatedAt: number;
  elapsedMs?: number | null;
}) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [active]);
  const elapsed = Math.max(0, active ? now - startedAt : (elapsedMs ?? updatedAt - startedAt));
  const seconds = Math.floor(elapsed / 1000);
  const text =
    seconds >= 60
      ? `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒`
      : elapsed < 1000 && !active
        ? `${elapsed} 毫秒`
        : `${seconds} 秒`;
  return (
    <span className="activity-time" aria-label={`耗时 ${text}`}>
      {text}
    </span>
  );
}

function payload(value: string): string {
  try {
    const parsed: unknown = JSON.parse(value);
    if (parsed !== null && typeof parsed === 'object') return JSON.stringify(parsed, null, 2);
  } catch {
    /* Plain text and exec source retain their original formatting. */
  }
  return value;
}

function ToolPayload({ item, active }: { item: TurnItem; active: boolean }) {
  return (
    <div className="activity-details">
      <div className="activity-detail-meta">
        <span>{item.name || toolName(item)}</span>
        <ElapsedTime
          active={active && item.status === 'in_progress'}
          startedAt={item.created_at}
          updatedAt={item.updated_at}
          elapsedMs={item.elapsed_ms}
        />
      </div>
      {item.input !== null && (
        <div className="activity-payload">
          <span className="activity-label">
            {item.kind === 'ptc_call' && toolName(item) === 'exec' ? '代码' : '输入'}
          </span>
          <pre>
            <code>{payload(item.input)}</code>
          </pre>
        </div>
      )}
      {item.output !== null && (
        <div className="activity-payload">
          <span className="activity-label">输出</span>
          <pre>
            <code>{payload(item.output)}</code>
          </pre>
        </div>
      )}
      {item.error && (
        <div className="activity-error" role="alert">
          {item.error}
        </div>
      )}
      {item.input === null && item.output === null && !item.error && (
        <span className="activity-label">
          {item.status === 'in_progress' && active ? '等待输出…' : '无输出'}
        </span>
      )}
      {item.truncated && <span className="activity-label">内容已截断</span>}
    </div>
  );
}

function ToolItem({ item, active }: { item: TurnItem; active: boolean }) {
  const label = operationLabel(item);
  return (
    <details className={`activity-tool ${item.status}`} data-testid={`item-${item.id}`}>
      <summary title={label}>
        <Icon name={item.status === 'failed' ? 'alert' : operation(item).icon} size={17} />
        <span className="activity-name">{label}</span>
        <Icon name="chevron" size={13} className="activity-chevron" />
      </summary>
      <ToolPayload item={item} active={active} />
    </details>
  );
}

function OperationGroup({
  item,
  tools,
  active,
}: {
  item: TurnItem;
  tools: TurnItem[];
  active: boolean;
}) {
  const failed = item.status === 'failed' || tools.some((tool) => tool.status === 'failed');
  const interrupted =
    item.status === 'interrupted' || tools.some((tool) => tool.status === 'interrupted');
  const [open, setOpen] = useState(failed || interrupted);
  useEffect(() => {
    if (failed || interrupted) setOpen(true);
  }, [failed, interrupted]);
  const label = groupLabel(item, tools);
  return (
    <details
      className={`activity-tool activity-group ${failed ? 'failed' : interrupted ? 'interrupted' : item.status}`}
      data-testid={`item-${item.id}`}
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary title={label}>
        <Icon
          name={failed ? 'alert' : tools.length === 1 ? operation(tools[0]).icon : 'terminal'}
          size={17}
        />
        <span className="activity-name">{label}</span>
        <Icon name="chevron" size={13} className="activity-chevron" />
      </summary>
      <div className="activity-group-content">
        <details
          className="activity-source"
          open={item.status === 'failed' || item.status === 'interrupted'}
        >
          <summary>
            <Icon name="code" size={14} />
            执行代码与输出
            <Icon name="chevron" size={12} className="activity-chevron" />
          </summary>
          <ToolPayload item={item} active={active} />
        </details>
        {tools.map((tool) => (
          <ToolItem key={tool.id} item={tool} active={active} />
        ))}
      </div>
    </details>
  );
}

function AssistantItem({ item }: { item: TurnItem }) {
  return (
    <div
      className={`activity-message ${item.phase || ''} ${item.status}`}
      data-testid={`item-${item.id}`}
    >
      {item.text && <Markdown>{item.text}</Markdown>}
      {item.status === 'in_progress' && <span className="streaming-caret" aria-label="正在输出" />}
      {(item.status === 'interrupted' || item.status === 'failed') && (
        <span className={`activity-message-status ${item.status}`}>
          {item.status === 'interrupted' ? '已中断' : '输出失败'}
        </span>
      )}
      {item.error && (
        <div className="activity-error" role="alert">
          {item.error}
        </div>
      )}
      {item.truncated && <span className="activity-label">内容已截断</span>}
    </div>
  );
}

function CopyAnswer({ text }: { text: string }) {
  const [result, setResult] = useState('');
  useEffect(() => {
    setResult('');
  }, [text]);
  useEffect(() => {
    if (!result) return;
    const timer = setTimeout(() => setResult(''), 2500);
    return () => clearTimeout(timer);
  }, [result]);
  return (
    <div className="answer-actions">
      <button
        className="icon-button"
        aria-label="复制回答"
        title="复制回答"
        onClick={async () => {
          try {
            await copyText(text);
            setResult('已复制');
          } catch {
            setResult('复制失败，请选择文本复制');
          }
        }}
      >
        <Icon name={result === '已复制' ? 'check' : 'copy'} size={17} />
      </button>
      <span role="status">{result}</span>
    </div>
  );
}

function FileResults({ items }: { items: TurnItem[] }) {
  const files = writtenFiles(items);
  if (!files.length) return null;
  return (
    <div className="file-results" aria-label="写入的文件">
      {files.map((file) => (
        <details className="file-result" data-testid={`file-result-${file.itemId}`} key={file.path}>
          <summary>
            <span className="file-result-icon">
              <Icon name="file" size={23} />
            </span>
            <span className="file-result-meta">
              <span className="file-result-path" title={file.path}>
                已写入 {file.path.split('/').filter(Boolean).at(-1) || file.path}
              </span>
              <span>{file.bytes.toLocaleString()} 字节</span>
            </span>
            <span className="file-result-action">
              查看内容
              <Icon name="chevron" size={14} />
            </span>
          </summary>
          <div className="file-result-content">
            <p className="file-result-full-path">{file.path}</p>
            <pre>
              <code>{file.content}</code>
            </pre>
            {file.truncated && <span className="activity-label">内容已截断</span>}
          </div>
        </details>
      ))}
    </div>
  );
}

export function TurnActivity({
  turn,
  state,
  loadMore,
  retry,
}: {
  turn: Turn;
  state?: TurnItemsState;
  loadMore: () => void;
  retry: () => void;
}) {
  const active = isActive(turn.status);
  const [open, setOpen] = useState(turn.status !== 'completed');
  useEffect(() => {
    // A completed turn becomes the concise answer. Failed and interrupted work
    // stays open so a missing final answer cannot conceal its partial progress.
    setOpen(turn.status !== 'completed');
  }, [turn.status]);
  const items = state?.items ?? [];
  const entries = activityEntries(items);
  const finals = items.filter(
    (item) => item.kind === 'assistant_message' && item.phase === 'final_answer',
  );
  const hasProgress = items.length > 0;
  const finalText = finals
    .map((item) => item.text || '')
    .filter(Boolean)
    .join('\n\n');
  const showFinalItems = finals.some(
    (item) => item.text || item.error || item.status !== 'completed',
  );
  const canonical = turn.status === 'completed' && turn.output !== null;
  const answer = canonical ? turn.output! : finalText || turn.output || '';
  const initialStatus =
    turn.status === 'queued' ? '排队中' : turn.status === 'cancelling' ? '正在中断' : '正在思考';
  const hasProcess = entries.length > 0 || Boolean(state?.hasMore || state?.error);
  return (
    <div className="turn-activity" aria-label="任务活动">
      {active && !hasProgress && !state?.error ? (
        <div className="turn-thinking" role="status">
          {initialStatus}
        </div>
      ) : (
        <details
          className={`turn-process${hasProcess ? '' : ' no-process'}`}
          data-testid={`turn-process-${turn.id}`}
          open={open}
          onToggle={(event) => setOpen(event.currentTarget.open)}
        >
          <summary aria-label={open ? '收起执行过程' : '展开执行过程'} aria-expanded={open}>
            <span>
              {turn.status === 'queued'
                ? '排队中'
                : turn.status === 'cancelling'
                  ? '正在中断'
                  : active
                    ? '已处理'
                    : turn.status === 'failed'
                      ? '执行失败 · 用时'
                      : turn.status === 'interrupted'
                        ? '已中断 · 用时'
                        : '用时'}
            </span>
            <ElapsedTime active={active} startedAt={turn.created_at} updatedAt={turn.updated_at} />
            <Icon name="chevron" size={14} className="process-chevron" />
          </summary>
          <div className="turn-process-content">
            {entries.map((entry) =>
              entry.kind === 'message' ? (
                <AssistantItem key={entry.item.id} item={entry.item} />
              ) : entry.tools.length ? (
                <OperationGroup
                  key={entry.item.id}
                  item={entry.item}
                  tools={entry.tools}
                  active={active}
                />
              ) : (
                <ToolItem key={entry.item.id} item={entry.item} active={active} />
              ),
            )}
            {!state?.loaded && state?.loading && (
              <p className="activity-label" role="status">
                正在加载执行过程…
              </p>
            )}
            {state?.hasMore && (
              <button
                className="text-button activity-more"
                onClick={loadMore}
                disabled={state.loading}
              >
                {state.loading ? '加载中…' : '加载更多活动'}
              </button>
            )}
            {!hasProcess && state?.loaded && <p className="activity-label">无执行记录</p>}
          </div>
        </details>
      )}
      {state?.error && (
        <div className="activity-load-error" role="alert">
          <span>{state.error}</span>
          <button className="text-button" onClick={retry} disabled={state.loading}>
            重试加载活动
          </button>
        </div>
      )}
      {canonical ? (
        answer && (
          <div
            className="activity-message final_answer"
            data-testid={
              finals.length ? `item-${finals[finals.length - 1].id}` : `turn-answer-${turn.id}`
            }
          >
            <Markdown>{answer}</Markdown>
          </div>
        )
      ) : showFinalItems ? (
        finals.map((item) => <AssistantItem key={item.id} item={item} />)
      ) : turn.output ? (
        <div className="activity-message final_answer" data-testid={`turn-answer-${turn.id}`}>
          <Markdown>{turn.output}</Markdown>
        </div>
      ) : null}
      {turn.error && (
        <div className="turn-error" role="alert">
          <Icon name="alert" size={16} />
          <span>{turn.error}</span>
        </div>
      )}
      {!answer && !turn.error && !active && state?.loaded && (
        <p className="terminal-note">
          {turn.status === 'interrupted'
            ? '任务已中断'
            : turn.status === 'failed'
              ? '任务执行失败。'
              : '任务已完成，没有文本结果。'}
        </p>
      )}
      {!active && <FileResults items={items} />}
      {!active && answer && <CopyAnswer text={answer} />}
    </div>
  );
}
