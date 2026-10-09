import { useEffect, useRef, useState } from 'react';
import type { AgentClient } from '../api/client';
import type { AgentSummary, ThreadCreateInput } from '../api/types';
import { threadRequest } from '../api/newThread';
import type { ThreadChoices } from '../api/newThread';
import { useRuntimeInstances } from './useRuntimeInstances';
import { Icon } from './Icon';
import './new-thread.css';

export function NewThread({
  client,
  agents,
  onClose,
  onCreate,
  busy,
  error,
}: {
  client: AgentClient;
  agents: AgentSummary[];
  onClose: () => void;
  onCreate: (input: ThreadCreateInput) => void;
  busy: boolean;
  error: string;
}) {
  const [agentId, setAgentId] = useState(agents[0]?.id ?? '');
  const [choices, setChoices] = useState<ThreadChoices>({
    title: '',
    mode: 'new',
    image: agents[0]?.runtime.default_image ?? '',
    workdir: agents[0]?.runtime.default_workdir ?? '/workspace',
    instanceId: '',
  });
  const [validationError, setValidationError] = useState('');
  const agent = agents.find((value) => value.id === agentId);
  const instances = useRuntimeInstances(client, agentId, choices.mode === 'reuse');
  const selected = instances.instances.find((value) => value.id === choices.instanceId);
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  const update = (patch: Partial<ThreadChoices>) => {
    setValidationError('');
    setChoices((previous) => ({ ...previous, ...patch }));
  };
  const changeAgent = (id: string) => {
    const next = agents.find((value) => value.id === id);
    setAgentId(id);
    update({
      image: next?.runtime.default_image ?? '',
      workdir: next?.runtime.default_workdir ?? '/workspace',
      instanceId: '',
    });
  };
  const selectInstance = (id: string) => {
    const instance = instances.instances.find((value) => value.id === id);
    update({
      instanceId: id,
      workdir: instance?.workdir ?? agent?.runtime.default_workdir ?? '/workspace',
    });
  };
  const canCreate =
    agent && !busy && (choices.mode === 'new' ? Boolean(choices.image) : Boolean(selected));

  return (
    <dialog
      ref={dialog}
      className="new-dialog runtime-dialog"
      aria-labelledby="new-thread-heading"
      onCancel={(event) => {
        if (busy) event.preventDefault();
        else onClose();
      }}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (busy) return;
          try {
            onCreate(threadRequest(agent, choices, selected));
          } catch (failure) {
            setValidationError(failure instanceof Error ? failure.message : '请检查会话配置');
          }
        }}
      >
        <div className="dialog-header">
          <h2 id="new-thread-heading">新建会话</h2>
          <button
            className="icon-button"
            type="button"
            aria-label="关闭新建会话"
            onClick={onClose}
            disabled={busy}
          >
            <Icon name="close" />
          </button>
        </div>
        <label htmlFor="agent-select">Agent</label>
        <select
          id="agent-select"
          value={agentId}
          onChange={(event) => changeAgent(event.target.value)}
          required
          disabled={busy || !agents.length}
        >
          {!agents.length && <option value="">暂无可用 Agent</option>}
          {agents.map((value) => (
            <option value={value.id} key={value.id}>
              {value.name}
            </option>
          ))}
        </select>
        <fieldset className="runtime-mode" disabled={busy}>
          <legend>运行环境</legend>
          {(['new', 'reuse'] as const).map((mode) => (
            <label key={mode} className={choices.mode === mode ? 'selected' : ''}>
              <input
                type="radio"
                name="runtime-mode"
                value={mode}
                checked={choices.mode === mode}
                onChange={() =>
                  update({
                    mode,
                    instanceId: '',
                    workdir: agent?.runtime.default_workdir ?? '/workspace',
                  })
                }
              />
              {mode === 'new' ? '新建实例' : '复用实例'}
            </label>
          ))}
        </fieldset>
        {choices.mode === 'new' ? (
          <>
            <label htmlFor="runtime-image">
              {agent?.runtime.provider === 'agentenv' ? '环境模板' : '镜像'}
            </label>
            <select
              id="runtime-image"
              value={choices.image}
              onChange={(event) => update({ image: event.target.value })}
              disabled={busy || !agent}
              required
            >
              {agent?.runtime.images.map((image) => (
                <option key={image} value={image}>
                  {image}
                  {image === agent.runtime.default_image ? '（默认）' : ''}
                </option>
              ))}
            </select>
          </>
        ) : (
          <>
            <label htmlFor="runtime-instance">已有实例</label>
            <select
              id="runtime-instance"
              value={choices.instanceId}
              onChange={(event) => selectInstance(event.target.value)}
              disabled={busy || (!instances.instances.length && instances.loading)}
              required
            >
              <option value="">
                {instances.loading && !instances.instances.length
                  ? '正在加载实例…'
                  : '选择已有实例'}
              </option>
              {instances.instances.map((instance) => (
                <option value={instance.id} key={instance.id}>
                  {instance.id.slice(0, 12)} · {instance.image} · {instance.workdir}
                  {instance.busy ? ' · 使用中' : ''}
                </option>
              ))}
            </select>
            {instances.error && (
              <div className="instance-error" role="alert">
                <span>{instances.error}</span>
                <button
                  type="button"
                  className="text-button"
                  onClick={instances.retry}
                  disabled={instances.loading}
                >
                  重试加载实例
                </button>
              </div>
            )}
            {!instances.loading && !instances.error && !instances.instances.length && (
              <p className="field-help">暂无可复用实例</p>
            )}
            {instances.nextCursor && (
              <button
                type="button"
                className="text-button instance-more"
                onClick={instances.loadMore}
                disabled={instances.loading || busy}
              >
                {instances.loading ? '加载中…' : '加载更多实例'}
              </button>
            )}
            {selected && (
              <>
                <label htmlFor="reused-image">
                  {selected.provider === 'agentenv' ? '环境模板' : '镜像'}
                  <span className="optional">沿用实例</span>
                </label>
                <input
                  id="reused-image"
                  value={selected.image}
                  readOnly
                  aria-label={selected.provider === 'agentenv' ? '环境模板' : '镜像'}
                />
                <p className="field-help">
                  {selected.busy ? '使用中，任务将排队执行。' : '沿用实例的 Agent 配置和文件。'}
                </p>
              </>
            )}
          </>
        )}
        <label htmlFor="thread-workdir">工作目录</label>
        <input
          id="thread-workdir"
          value={choices.workdir}
          onChange={(event) => update({ workdir: event.target.value })}
          placeholder="/workspace"
          disabled={busy}
          required
          spellCheck={false}
          autoComplete="off"
        />
        <label htmlFor="thread-title">
          会话名称 <span className="optional">可选</span>
        </label>
        <input
          id="thread-title"
          value={choices.title}
          onChange={(event) => update({ title: event.target.value })}
          maxLength={120}
          disabled={busy}
          placeholder="例如：梳理项目结构"
        />
        {(validationError || error) && (
          <div role="alert" className="error-box">
            <Icon name="alert" />
            <span>{validationError || error}</span>
          </div>
        )}
        <button className="button primary wide" disabled={!canCreate} type="submit">
          {busy ? (
            <>
              <span className="spinner" />
              正在创建
            </>
          ) : (
            <>
              创建会话
              <Icon name="arrow" size={16} />
            </>
          )}
        </button>
      </form>
    </dialog>
  );
}
