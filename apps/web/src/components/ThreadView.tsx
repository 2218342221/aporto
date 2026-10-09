import { useEffect, useRef, useState } from 'react';
import type { FormEvent } from 'react';
import type { AgentClient } from '../api/client';
import type {
  AgentEvent,
  AgentSummary,
  ConnectionState,
  Thread,
  ThreadReadResult,
  Turn,
} from '../api/types';
import { isActive } from '../api/types';
import { coalescedRefresh, mergeSnapshot, mergeTurns } from '../api/state';
import { createIdempotencyKey } from '../api/idempotency';
import type { TurnItemsState } from '../api/items';
import { isItemEvent } from '../api/validation';
import { BrandMark, Icon } from './Icon';
import { fullTime, timeLabel } from './format';
import { useTurnItems } from './useTurnItems';
import { TurnActivity } from './TurnActivity';

const errorText = (error: unknown) =>
  error instanceof Error ? error.message : '请求失败，请稍后重试';
const eventNames: Record<string, string> = {
  'thread.started': '会话已创建',
  'turn.queued': '任务进入队列',
  'turn.started': '开始执行任务',
  'turn.running': '开始执行任务',
  'turn.cancelling': '正在中断任务',
  'turn.completed': '任务已完成',
  'turn.failed': '任务执行失败',
  'turn.interrupted': '任务已中断',
  'runtime.ready': '运行环境已就绪',
  'runtime.paused': '运行环境已暂停',
  'runtime.failed': '运行环境连接失败',
  'model.started': 'Agent 正在思考',
  'model.completed': '模型响应完成',
  'model.failed': '任务处理失败',
  'ptc.started': '开始执行程序',
  'ptc.completed': '程序执行结束',
  'tool.started': '开始调用工具',
  'tool.completed': '工具调用完成',
  'tool.initialization_failed': '工具初始化失败',
  'tool.cleanup_failed': '工具清理失败',
};
const connectionNames: Record<ConnectionState, string> = {
  connecting: '正在连接',
  connected: '实时连接',
  reconnecting: '重新连接中',
  disconnected: '连接已断开',
};

function EventPanel({
  events,
  state,
  error,
  retry,
  close,
}: {
  events: AgentEvent[];
  state: ConnectionState;
  error: string;
  retry: () => void;
  close: () => void;
}) {
  return (
    <aside className="event-panel" aria-label="执行进度">
      <div className="event-panel-heading">
        <div>
          <Icon name="bolt" size={17} />
          <h2>执行进度</h2>
        </div>
        <button className="icon-button" onClick={close} aria-label="关闭执行进度">
          <Icon name="close" size={16} />
        </button>
      </div>
      <div className={`stream-status ${state}`} role="status">
        <span className="status-dot" />
        {connectionNames[state]}
      </div>
      {error && (
        <div className="stream-error" role="alert">
          <span>{error}</span>
          {state === 'disconnected' && (
            <button className="text-button" onClick={retry}>
              重新连接
            </button>
          )}
        </div>
      )}
      <div className="event-list">
        {events.length ? (
          [...events].reverse().map((event) => (
            <article
              className={`event-item ${event.kind.endsWith('failed') ? 'error' : ''}`}
              key={event.sequence}
              data-testid={`event-${event.sequence}`}
            >
              <div className={`event-marker ${event.kind.endsWith('completed') ? 'complete' : ''}`}>
                <Icon
                  name={
                    event.kind.endsWith('failed')
                      ? 'alert'
                      : event.kind.endsWith('completed')
                        ? 'check'
                        : event.kind.startsWith('tool.')
                          ? 'code'
                          : 'bolt'
                  }
                  size={12}
                />
              </div>
              <div className="event-content">
                <div>
                  <strong title={`${event.kind} #${event.sequence}`}>
                    {eventNames[event.kind] || '状态已更新'}
                  </strong>
                  <time
                    title={fullTime(event.created_at)}
                    dateTime={new Date(event.created_at).toISOString()}
                  >
                    {timeLabel(event.created_at)}
                  </time>
                </div>
              </div>
            </article>
          ))
        ) : (
          <div className="events-empty">
            <Icon name="clock" size={28} />
            <p>等待任务开始</p>
          </div>
        )}
      </div>
    </aside>
  );
}

function MessageTurn({
  turn,
  agentName,
  activities,
  loadMore,
  retry,
}: {
  turn: Turn;
  agentName: string;
  activities?: TurnItemsState;
  loadMore: () => void;
  retry: () => void;
}) {
  return (
    <div className="turn" data-testid={`turn-${turn.id}`}>
      <article className="message user-message" aria-label="你的消息">
        <div className="user-text" title={fullTime(turn.created_at)}>
          {turn.input}
        </div>
      </article>
      <article
        className={`message assistant-message status-${turn.status}`}
        aria-label={`${agentName}的回复`}
      >
        <div className="message-body">
          <TurnActivity turn={turn} state={activities} loadMore={loadMore} retry={retry} />
        </div>
      </article>
    </div>
  );
}

export function ThreadView({
  client,
  threadId,
  agent,
  title,
  showEvents,
  onCloseEvents,
  onThread,
}: {
  client: AgentClient;
  threadId: string;
  agent?: AgentSummary;
  title: string;
  showEvents: boolean;
  onCloseEvents: () => void;
  onThread: (thread: Thread) => void;
}) {
  const [snapshot, setSnapshot] = useState<ThreadReadResult | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState('');
  const [actionError, setActionError] = useState('');
  const [events, setEvents] = useState<AgentEvent[]>([]);
  const [streamState, setStreamState] = useState<ConnectionState>('connecting');
  const [streamError, setStreamError] = useState('');
  const [connectionAttempt, setConnectionAttempt] = useState(0);
  const [connectionEpoch, setConnectionEpoch] = useState(0);
  const activities = useTurnItems(client, threadId, snapshot?.turns ?? [], connectionEpoch);
  const [draft, setDraft] = useState('');
  const [sending, setSending] = useState(false);
  const [interrupting, setInterrupting] = useState(false);
  const [loadingHistory, setLoadingHistory] = useState(false);
  const controllerRef = useRef<AbortController | null>(null);
  const refreshRef = useRef<() => Promise<void>>(async () => {});
  const sequenceRef = useRef(0);
  const requestVersion = useRef(0);
  const submission = useRef<{ input: string; key: string } | null>(null);
  const messages = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const composer = useRef<HTMLTextAreaElement>(null);
  const sendingRef = useRef(false);

  useEffect(() => {
    const controller = new AbortController();
    controllerRef.current = controller;
    const { signal } = controller;
    sendingRef.current = false;
    setSending(false);
    setInterrupting(false);
    setLoadingHistory(false);
    let refreshTimer: ReturnType<typeof setTimeout> | undefined;
    const refresh = coalescedRefresh(async () => {
      const version = ++requestVersion.current;
      try {
        const result = await client.thread(threadId, signal);
        if (signal.aborted || version !== requestVersion.current) return;
        setSnapshot((previous) => mergeSnapshot(previous, result));
        setLoadError('');
        setLoading(false);
        onThread(result.thread);
      } catch (error) {
        if (!signal.aborted && version === requestVersion.current) {
          setLoadError(errorText(error));
          setLoading(false);
        }
      }
    }, signal);
    refreshRef.current = refresh;
    void refresh();
    void client.events(threadId, {
      signal,
      after: sequenceRef.current,
      onEvent: (event) => {
        if (signal.aborted) return;
        sequenceRef.current = event.sequence;
        activities.receive(event);
        if (isItemEvent(event.kind)) return;
        setEvents((previous) => [...previous, event].slice(-150));
        // Coalesce a burst of tool events into one canonical conversation read.
        if (!refreshTimer)
          refreshTimer = setTimeout(() => {
            refreshTimer = undefined;
            void refresh();
          }, 120);
      },
      onState: (state) => {
        if (!signal.aborted) {
          setStreamState(state);
          if (state === 'connected') {
            setStreamError('');
            setConnectionEpoch((value) => value + 1);
            // An earlier snapshot may have failed after the last delivered event.
            // A healthy connection must also reconcile the durable conversation.
            void refresh();
          }
        }
      },
      onError: (error) => {
        if (!signal.aborted) setStreamError(error.message);
      },
    });
    return () => {
      controller.abort();
      if (refreshTimer) clearTimeout(refreshTimer);
    };
  }, [client, threadId, connectionAttempt, onThread, activities.receive]);

  useEffect(() => {
    if (!pinned.current) return;
    const frame = requestAnimationFrame(() => {
      if (pinned.current && messages.current)
        messages.current.scrollTop = messages.current.scrollHeight;
    });
    return () => cancelAnimationFrame(frame);
  }, [snapshot?.turns, activities.state]);

  const active = snapshot?.turns.findLast((turn) => isActive(turn.status));
  const submit = async (event?: FormEvent) => {
    event?.preventDefault();
    const input = draft.trim();
    if (!input || active || sendingRef.current || !snapshot) return;
    const signal = controllerRef.current!.signal;
    const submittedDraft = draft;
    sendingRef.current = true;
    setSending(true);
    setActionError('');
    try {
      if (submission.current?.input !== input)
        submission.current = { input, key: createIdempotencyKey() };
      const turn = await client.startTurn(threadId, input, submission.current.key, signal);
      if (signal.aborted) return;
      requestVersion.current++;
      pinned.current = true;
      setSnapshot(
        (previous) => previous && { ...previous, turns: mergeTurns(previous.turns, [turn]) },
      );
      setDraft((value) => (value === submittedDraft ? '' : value));
      submission.current = null;
      void refreshRef.current();
      composer.current?.focus();
    } catch (error) {
      if (!signal.aborted) setActionError(errorText(error));
    } finally {
      if (!signal.aborted) {
        sendingRef.current = false;
        setSending(false);
      }
    }
  };
  const interrupt = async () => {
    if (!active || interrupting) return;
    const signal = controllerRef.current!.signal;
    setInterrupting(true);
    setActionError('');
    try {
      const turn = await client.interrupt(threadId, active.id, signal);
      if (signal.aborted) return;
      requestVersion.current++;
      setSnapshot(
        (previous) => previous && { ...previous, turns: mergeTurns(previous.turns, [turn]) },
      );
      void refreshRef.current();
    } catch (error) {
      if (!signal.aborted) setActionError(errorText(error));
    } finally {
      if (!signal.aborted) setInterrupting(false);
    }
  };
  const loadHistory = async () => {
    if (!snapshot?.next_cursor || loadingHistory) return;
    const signal = controllerRef.current!.signal;
    const cursor = snapshot.next_cursor;
    const height = messages.current?.scrollHeight ?? 0;
    pinned.current = false;
    setLoadingHistory(true);
    setActionError('');
    try {
      const result = await client.thread(threadId, signal, cursor);
      if (signal.aborted) return;
      setSnapshot(
        (previous) =>
          previous && {
            ...previous,
            turns: mergeTurns(previous.turns, result.turns),
            // A concurrent reconnect may discover a newer history gap.
            next_cursor:
              previous.next_cursor === cursor ? result.next_cursor : previous.next_cursor,
          },
      );
      requestAnimationFrame(() => {
        if (!signal.aborted && messages.current)
          messages.current.scrollTop += messages.current.scrollHeight - height;
      });
    } catch (error) {
      if (!signal.aborted) setActionError(errorText(error));
    } finally {
      if (!signal.aborted) setLoadingHistory(false);
    }
  };

  return (
    <div className={`thread-layout${showEvents ? ' with-events' : ''}`}>
      <div className="conversation">
        <div className="conversation-heading">
          <div>
            <h1 title={title}>{title}</h1>
            {snapshot && (
              <p
                className="conversation-runtime"
                title={`${snapshot.thread.runtime_provider} · ${snapshot.thread.runtime_image} · ${snapshot.thread.workdir}`}
              >
                <span>{snapshot.thread.runtime_image}</span>
                <span aria-hidden="true">·</span>
                <span>{snapshot.thread.workdir}</span>
              </p>
            )}
          </div>
          <span
            className="agent-badge"
            title={snapshot ? `Release: ${snapshot.thread.release_id}` : undefined}
          >
            <Icon name="box" size={14} />
            <span className="agent-badge-label">
              {agent?.name || snapshot?.thread.agent_id || 'Agent'}
              {agent && agent.release_id === snapshot?.thread.release_id ? ` · ${agent.model}` : ''}
            </span>
          </span>
        </div>
        {streamState !== 'connected' && (
          <div className={`connection-banner ${streamState}`} role="status">
            <span className="status-dot" />
            {streamState === 'disconnected'
              ? '实时进度已断开'
              : streamState === 'reconnecting'
                ? '正在重新连接…'
                : '正在连接实时进度…'}
            {streamState === 'disconnected' && (
              <button
                onClick={() => setConnectionAttempt((value) => value + 1)}
                className="text-button"
              >
                重新连接
              </button>
            )}
          </div>
        )}
        <div
          className="messages"
          ref={messages}
          onScroll={() => {
            if (messages.current)
              pinned.current =
                messages.current.scrollHeight -
                  messages.current.scrollTop -
                  messages.current.clientHeight <
                120;
          }}
          aria-label="会话内容"
        >
          {loading && (
            <div className="conversation-loading" role="status">
              <span className="spinner" />
              正在加载会话…
            </div>
          )}
          {loadError && (
            <div className="read-error error-box" role="alert">
              <Icon name="alert" />
              <div>
                {loadError}
                <button className="text-button" onClick={() => void refreshRef.current()}>
                  重试加载
                </button>
              </div>
            </div>
          )}
          {snapshot?.next_cursor && (
            <div className="history-control">
              <button className="text-button" onClick={loadHistory} disabled={loadingHistory}>
                {loadingHistory ? '正在加载更早的消息…' : '加载更早的消息'}
              </button>
            </div>
          )}
          {!loading && snapshot && !snapshot.turns.length && (
            <div className="conversation-empty">
              <BrandMark large />
              <h2>发送消息开始</h2>
            </div>
          )}
          {snapshot?.turns.map((turn) => (
            <MessageTurn
              key={turn.id}
              turn={turn}
              agentName={agent?.name || 'Agent'}
              activities={activities.state[turn.id]}
              loadMore={() => {
                pinned.current = false;
                void activities.loadMore(turn.id);
              }}
              retry={() => void activities.retry(turn.id)}
            />
          ))}
        </div>
        <div className="composer-area">
          {actionError && (
            <div className="action-error error-box" role="alert">
              <Icon name="alert" size={16} />
              <span>{actionError}</span>
              <button
                className="icon-button"
                onClick={() => setActionError('')}
                aria-label="关闭错误提示"
              >
                <Icon name="close" size={14} />
              </button>
            </div>
          )}
          <form className={`composer${active ? ' busy' : ''}`} onSubmit={submit}>
            <textarea
              ref={composer}
              aria-label="发送给 Agent 的消息"
              aria-keyshortcuts="Control+Enter Meta+Enter"
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              rows={3}
              maxLength={100_000}
              disabled={!snapshot}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && (e.metaKey || e.ctrlKey) && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  void submit();
                }
              }}
            />
            <div className="composer-toolbar">
              {active ? (
                <button
                  className="send-button"
                  type="button"
                  aria-label={
                    interrupting || active.status === 'cancelling' ? '正在中断' : '中断任务'
                  }
                  title="中断任务"
                  onClick={interrupt}
                  disabled={interrupting || active.status === 'cancelling'}
                >
                  {interrupting || active.status === 'cancelling' ? (
                    <span className="spinner" />
                  ) : (
                    <Icon name="stop" size={16} />
                  )}
                </button>
              ) : (
                <button
                  className="send-button"
                  type="submit"
                  aria-label="发送消息"
                  title="发送消息"
                  disabled={!draft.trim() || !snapshot || sending}
                >
                  {sending ? <span className="spinner" /> : <Icon name="arrow" size={18} />}
                </button>
              )}
            </div>
          </form>
        </div>
      </div>
      {showEvents && (
        <EventPanel
          events={events}
          state={streamState}
          error={streamError}
          retry={() => setConnectionAttempt((value) => value + 1)}
          close={onCloseEvents}
        />
      )}
    </div>
  );
}
