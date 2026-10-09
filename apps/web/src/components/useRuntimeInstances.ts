import { useEffect, useMemo, useRef, useState } from 'react';
import type { AgentClient } from '../api/client';
import type { RuntimeInstance } from '../api/types';
import { mergeInstances } from '../api/newThread';

interface InstancePage {
  instances: RuntimeInstance[];
  nextCursor: string | null;
  loading: boolean;
  error: string;
}
const emptyPage = (): InstancePage => ({
  instances: [],
  nextCursor: null,
  loading: false,
  error: '',
});

export function useRuntimeInstances(client: AgentClient, agentId: string, enabled: boolean) {
  const scope = useMemo(() => ({ client, agentId, enabled }), [client, agentId, enabled]);
  const [snapshot, setSnapshot] = useState({ scope, page: emptyPage() });
  const actions = useRef({ more: () => {}, retry: () => {} });

  useEffect(() => {
    const controller = new AbortController();
    const initial = emptyPage();
    let page = initial;
    let pending = false;
    const read = async (cursor?: string) => {
      if (pending || controller.signal.aborted) return;
      pending = true;
      page = { ...page, loading: true, error: '' };
      setSnapshot({ scope, page });
      try {
        const result = await client.instances(agentId, controller.signal, cursor);
        if (controller.signal.aborted) return;
        page = {
          instances: mergeInstances(page.instances, result.instances),
          nextCursor: result.next_cursor,
          loading: false,
          error: '',
        };
        setSnapshot({ scope, page });
      } catch (error) {
        if (!controller.signal.aborted) {
          page = {
            ...page,
            loading: false,
            error: error instanceof Error ? error.message : '实例加载失败',
          };
          setSnapshot({ scope, page });
        }
      } finally {
        pending = false;
      }
    };
    actions.current = {
      more: () => {
        if (page.nextCursor) void read(page.nextCursor);
      },
      retry: () => {
        if (enabled && agentId) void read(page.nextCursor ?? undefined);
      },
    };
    setSnapshot({ scope, page });
    if (enabled && agentId) void read();
    return () => controller.abort();
  }, [client, agentId, enabled, scope]);

  // Switching Agent or mode immediately hides the previous scope, before effects run.
  const page = snapshot.scope === scope ? snapshot.page : { ...emptyPage(), loading: enabled };
  return { ...page, loadMore: () => actions.current.more(), retry: () => actions.current.retry() };
}
