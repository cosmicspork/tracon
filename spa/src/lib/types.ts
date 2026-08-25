// Mirrors the node's serde output. Field names must match exactly; the node is
// the source of truth and the interface renders what it is given.

export interface NodeInfo {
  id: string
  name: string
  state: 'ready' | 'refused' | 'unknown'
  failed_check: string | null
  failed_detail: string | null
  harness: { id: string; pinned: string; found: string | null; mismatch: boolean }
  models: ModelOption[]
  checked_at_ms: number | null
}

export interface ModelOption {
  value: string
  name: string
}

export type SessionState =
  | 'starting'
  | 'running'
  | 'waiting_on_you'
  | 'waiting_on_check'
  | 'closed'
  | 'killed_budget'
  | 'failed'

export interface Session {
  id: string
  node_id: string
  channel: string
  work_item_id: string | null
  repo_path: string
  worktree_path: string | null
  branch: string
  harness_id: string
  harness_version: string
  model: string
  budget_tokens: number
  tokens_used: number
  cost_usd: number | null
  context_used: number | null
  context_size: number | null
  state: SessionState
  end_reason: string | null
  last_error: string | null
  turn_active: number
  draft: string | null
  created_ms: number
  updated_ms: number
}

export interface Event {
  seq: number
  session_id: string
  kind: string
  ref_id: string | null
  payload: Record<string, unknown>
  at_ms: number
  mono_ms: number
}

export interface Permission {
  id: string
  session_id: string
  title: string
  kind: string | null
  raw_input: string | null
  options: string
  state: 'new' | 'answered' | 'expired'
  created_ms: number
  expires_ms: number
}

export interface Review {
  id: string
  session_id: string
  channel: string
  kind: string
  title: string
  body: string
  edited_title: string | null
  edited_body: string | null
  provider: string
  target: string
  diff: string
  files: string
  head_sha: string
  base_ref: string
  added: number
  removed: number
  state: 'new' | 'claimed' | 'revising' | 'approved' | 'rejected'
  verdict_reason: string | null
  publish_result: string | null
  claimed_ms: number | null
  created_ms: number
}

export interface Queue {
  waiting: Permission[]
  reviews: Review[]
  running: Session[]
  ended: Session[]
}

export type Frame =
  | ({ type: 'event' } & Event)
  | { type: 'chunk'; session_id: string; message_id: string | null; kind: string; text: string }
  | { type: 'tool_update'; session_id: string; tool_call_id: string; status: string | null }
  | ({ type: 'session' } & Session)
  | { type: 'queue'; waiting: Permission[] }
  | { type: 'reviews'; waiting: Review[] }
  | { type: 'node' }

export const TERMINAL_STATES: SessionState[] = ['closed', 'killed_budget', 'failed']

export function isTerminal(state: SessionState): boolean {
  return TERMINAL_STATES.includes(state)
}

export function permissionOptions(p: Permission): { option_id: string; name: string; kind: string }[] {
  // The node stores the options as the harness sent them, which for ACP is
  // camelCase; the interface normalises rather than assuming.
  try {
    const raw = JSON.parse(p.options) as Record<string, string>[]
    return raw.map((o) => ({
      option_id: o.option_id ?? o.optionId ?? '',
      name: o.name ?? '',
      kind: o.kind ?? '',
    }))
  } catch {
    return []
  }
}
