import { useCallback, useEffect, useRef, useState } from 'react';
import type { FormEvent } from 'react';
import { AgentClient } from './api/client';
import { mergeThreads } from './api/state';
import type { AgentSummary, Thread, ThreadCreateInput, ThreadListResult } from './api/types';
import { BrandMark, Icon } from './components/Icon';
import { shortDate, fullTime } from './components/format';
import { ThreadView } from './components/ThreadView';
import { NewThread } from './components/NewThread';

interface Session {
  client: AgentClient;
  agents: AgentSummary[];
  initial: ThreadListResult;
}
const errorText = (error: unknown) =>
  error instanceof Error ? error.message : '请求失败，请稍后重试';

function Login({ onConnect }: { onConnect: (session: Session) => void }) {
  const [url, setUrl] = useState<string>(
    import.meta.env.VITE_APORTO_API_URL || 'http://127.0.0.1:8080',
  );
  const [token, setToken] = useState('');
  const [pending, setPending] = useState(false);
  const [error, setError] = useState('');
  const request = useRef<AbortController | null>(null);
  useEffect(() => () => request.current?.abort(), []);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    request.current?.abort();
    const controller = new AbortController();
    request.current = controller;
    setPending(true);
    setError('');
    try {
      const client = new AgentClient(url, token.trim());
      const [agents, initial] = await Promise.all([
        client.agents(controller.signal),
        client.threads(controller.signal),
      ]);
      if (!controller.signal.aborted) {
        setToken('');
        onConnect({ client, agents: agents.agents, initial });
      }
    } catch (failure) {
      if (!controller.signal.aborted) setError(errorText(failure));
    } finally {
      if (!controller.signal.aborted) setPending(false);
    }
  };
  return (
    <main className="login-page">
      <div className="login-layout">
        <div className="login-brand">
          <span className="brand-wordmark">Aporto</span>
          <h1 id="login-heading">连接工作台</h1>
        </div>
        <section className="login-card" aria-labelledby="login-heading">
          <form onSubmit={submit}>
            <label htmlFor="service-url">服务地址</label>
            <input
              id="service-url"
              type="url"
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              required
              spellCheck={false}
              placeholder="https://aporto.example.com"
              autoComplete="url"
            />
            <label htmlFor="service-token">访问令牌</label>
            <input
              id="service-token"
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              required
              autoComplete="off"
              placeholder="输入访问令牌"
            />
            <p className="field-help">刷新后需重新输入令牌。</p>
            {error && (
              <div className="error-box" role="alert">
                <Icon name="alert" />
                <span>{error}</span>
              </div>
            )}
            <button className="button primary wide" type="submit" disabled={pending}>
              {pending ? (
                <>
                  <span className="spinner" />
                  正在连接
                </>
              ) : (
                <>
                  进入工作台
                  <Icon name="chevron" size={16} />
                </>
              )}
            </button>
          </form>
        </section>
      </div>
    </main>
  );
}

function Workbench({ session, onDisconnect }: { session: Session; onDisconnect: () => void }) {
  const { client, agents } = session;
  const [threads, setThreads] = useState(mergeThreads([], session.initial.threads));
  const [nextCursor, setNextCursor] = useState(session.initial.next_cursor);
  const [selected, setSelected] = useState<string | null>(session.initial.threads[0]?.id ?? null);
  const [search, setSearch] = useState('');
  const [showNew, setShowNew] = useState(false);
  const [creating, setCreating] = useState(false);
  const [creationError, setCreationError] = useState('');
  const [listError, setListError] = useState('');
  const [loadingMore, setLoadingMore] = useState(false);
  const [mobileSidebar, setMobileSidebar] = useState(false);
  const [showEvents, setShowEvents] = useState(false);
  const controller = useRef<AbortController | null>(null);
  useEffect(() => {
    const active = new AbortController();
    controller.current = active;
    return () => active.abort();
  }, []);
  const updateThread = useCallback(
    (thread: Thread) => setThreads((previous) => mergeThreads(previous, [thread])),
    [],
  );
  const create = async (input: ThreadCreateInput) => {
    const signal = controller.current!.signal;
    setCreating(true);
    setCreationError('');
    try {
      const thread = await client.createThread(input, signal);
      if (signal.aborted) return;
      updateThread(thread);
      setSelected(thread.id);
      setShowNew(false);
      setMobileSidebar(false);
    } catch (error) {
      if (!signal.aborted) setCreationError(errorText(error));
    } finally {
      if (!signal.aborted) setCreating(false);
    }
  };
  const moreThreads = async () => {
    if (!nextCursor || loadingMore) return;
    const signal = controller.current!.signal;
    setLoadingMore(true);
    setListError('');
    try {
      const result = await client.threads(signal, nextCursor);
      if (signal.aborted) return;
      setThreads((current) => mergeThreads(current, result.threads));
      setNextCursor(result.next_cursor);
    } catch (error) {
      if (!signal.aborted) setListError(errorText(error));
    } finally {
      if (!signal.aborted) setLoadingMore(false);
    }
  };
  const currentThread = threads.find((t) => t.id === selected);
  const agent = agents.find((a) => a.id === currentThread?.agent_id);
  const openNew = () => {
    setCreationError('');
    setShowNew(true);
  };
  return (
    <div className={`workbench${mobileSidebar ? ' sidebar-open' : ''}`}>
      {mobileSidebar && (
        <button
          className="sidebar-backdrop"
          aria-label="收起会话列表"
          onClick={() => setMobileSidebar(false)}
        />
      )}
      <aside id="thread-sidebar" className="sidebar" aria-label="会话列表">
        <div className="brand">
          <BrandMark />
          <div className="brand-wordmark">Aporto</div>
          <button
            className="icon-button mobile-only sidebar-close"
            onClick={() => setMobileSidebar(false)}
            aria-label="关闭会话列表"
          >
            <Icon name="close" />
          </button>
        </div>
        <button className="button new-thread" onClick={openNew}>
          <Icon name="plus" size={19} />
          新建会话
        </button>
        <div className="sidebar-search">
          <Icon name="search" size={15} />
          <input
            aria-label="搜索已加载的会话"
            placeholder="搜索会话"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </div>
        <div className="sidebar-label">
          最近会话<span>{threads.length}</span>
        </div>
        <nav className="thread-list">
          {threads
            .filter((thread) =>
              thread.title.toLocaleLowerCase().includes(search.toLocaleLowerCase()),
            )
            .map((thread) => (
              <button
                key={thread.id}
                className={`thread-item${selected === thread.id ? ' selected' : ''}`}
                onClick={() => {
                  setSelected(thread.id);
                  setMobileSidebar(false);
                }}
                aria-current={selected === thread.id ? 'page' : undefined}
              >
                <Icon name="chat" size={17} />
                <div>
                  <span>{thread.title || '未命名会话'}</span>
                  <small>
                    {agents.find((a) => a.id === thread.agent_id)?.name || thread.agent_id}
                    <time title={fullTime(thread.updated_at)}>{shortDate(thread.updated_at)}</time>
                  </small>
                </div>
              </button>
            ))}
          {!threads.length && <p className="sidebar-empty">暂无会话</p>}
          {search &&
            !threads.some((t) =>
              t.title.toLocaleLowerCase().includes(search.toLocaleLowerCase()),
            ) && <p className="sidebar-empty">已加载的会话中没有匹配项。</p>}
          {nextCursor && (
            <button
              className="text-button load-threads"
              onClick={moreThreads}
              disabled={loadingMore}
            >
              {loadingMore ? '正在加载…' : '加载更多会话'}
            </button>
          )}
          {listError && (
            <p className="small-error" role="alert">
              {listError}
            </p>
          )}
        </nav>
        <div className="sidebar-bottom">
          <div className="workspace-info">
            <span className="workspace-avatar">
              <Icon name="box" size={19} />
            </span>
            <div>
              <span title={client.baseUrl}>{new URL(client.baseUrl).host}</span>
            </div>
            <span className="status-dot" />
          </div>
          <button className="disconnect-button" onClick={onDisconnect}>
            <Icon name="logout" size={15} />
            断开连接
          </button>
        </div>
      </aside>
      <section className="workspace-main">
        <header className="workspace-header">
          <div className="breadcrumb">
            <button
              className="icon-button mobile-only"
              onClick={() => setMobileSidebar(true)}
              aria-label="打开会话列表"
              aria-expanded={mobileSidebar}
              aria-controls="thread-sidebar"
            >
              <Icon name="menu" />
            </button>
            <strong>{selected ? '会话' : '工作台'}</strong>
          </div>
          <div className="header-actions">
            <button
              className={`icon-button${showEvents ? ' active' : ''}`}
              onClick={() => setShowEvents((value) => !value)}
              aria-label={showEvents ? '收起执行进度' : '展开执行进度'}
              aria-pressed={showEvents}
            >
              <Icon name="panel" size={19} />
            </button>
          </div>
        </header>
        {selected ? (
          <ThreadView
            key={selected}
            client={client}
            threadId={selected}
            agent={agent}
            title={currentThread?.title || '未命名会话'}
            showEvents={showEvents}
            onCloseEvents={() => setShowEvents(false)}
            onThread={updateThread}
          />
        ) : (
          <div className="empty-workspace">
            <div className="empty-hero">
              <div className="hero-emblem">
                <BrandMark large />
              </div>
              <h1>暂无会话</h1>
              <button className="button primary" onClick={openNew}>
                <Icon name="plus" />
                新建会话
              </button>
            </div>
          </div>
        )}
      </section>
      {showNew && (
        <NewThread
          client={client}
          agents={agents}
          onClose={() => setShowNew(false)}
          onCreate={create}
          busy={creating}
          error={creationError}
        />
      )}
    </div>
  );
}

export default function App() {
  // Deliberately memory-only: no token in URLs, storage, cookies or build configuration.
  const [session, setSession] = useState<Session | null>(null);
  return session ? (
    <Workbench session={session} onDisconnect={() => setSession(null)} />
  ) : (
    <Login onConnect={setSession} />
  );
}
