// Mirrors crates/protocol/src/lib.rs. All timestamps are Unix milliseconds.
export interface AgentSummary {
  id: string;
  name: string;
  model: string;
  bundle_digest: string;
  release_id: string;
  runtime: {
    provider: 'docker' | 'agentenv';
    images: string[];
    default_image: string;
    default_workdir: string;
  };
}
export interface AgentListResult {
  agents: AgentSummary[];
}
export interface Thread {
  id: string;
  title: string;
  agent_id: string;
  bundle_digest: string;
  release_id: string;
  sandbox_id: string | null;
  runtime_instance_id: string;
  runtime_image: string;
  runtime_provider: string;
  workdir: string;
  created_at: number;
  updated_at: number;
  last_sequence: number;
}
export interface ThreadListResult {
  threads: Thread[];
  next_cursor: string | null;
}
export interface RuntimeInstance {
  id: string;
  agent_id: string;
  release_id: string;
  bundle_digest: string;
  provider: string;
  image: string;
  sandbox_id: string;
  workdir: string;
  busy: boolean;
  created_at: number;
  updated_at: number;
}
export interface RuntimeInstanceListResult {
  instances: RuntimeInstance[];
  next_cursor: string | null;
}
export interface ThreadCreateInput {
  agent_id: string;
  title?: string | null;
  runtime?: { mode: 'new'; image?: string } | { mode: 'reuse'; instance_id: string };
  workdir?: string;
}
export type TurnStatus =
  | 'queued'
  | 'running'
  | 'cancelling'
  | 'completed'
  | 'failed'
  | 'interrupted';
export interface Turn {
  id: string;
  thread_id: string;
  input: string;
  status: TurnStatus;
  output: string | null;
  error: string | null;
  created_at: number;
  updated_at: number;
}
export interface ThreadReadResult {
  thread: Thread;
  turns: Turn[];
  next_cursor: string | null;
}
export type TurnItemStatus = 'in_progress' | 'completed' | 'failed' | 'interrupted';
export interface TurnItem {
  id: string;
  turn_id: string;
  /** Immutable creation cursor; updates retain this position in the transcript. */
  ordinal: number;
  sequence: number;
  created_at: number;
  updated_at: number;
  kind: 'assistant_message' | 'ptc_call' | 'tool_call';
  status: TurnItemStatus;
  phase: 'commentary' | 'final_answer' | null;
  name: string | null;
  text: string | null;
  input: string | null;
  output: string | null;
  error: string | null;
  elapsed_ms: number | null;
  truncated: boolean;
}
export interface TurnItemListResult {
  items: TurnItem[];
  next_cursor: number;
  has_more: boolean;
}
export interface AgentEvent {
  sequence: number;
  thread_id: string;
  turn_id: string | null;
  kind: string;
  data: unknown;
  created_at: number;
}
export type ConnectionState = 'connecting' | 'connected' | 'reconnecting' | 'disconnected';
export const isActive = (status: TurnStatus) =>
  ['queued', 'running', 'cancelling'].includes(status);
