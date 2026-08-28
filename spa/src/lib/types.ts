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
  /** The node that served this interface. */
  is_self: boolean
  /** False once the hub has not heard from a peer within the presence window. */
  reachable: boolean
  last_seen_ms: number | null
  x25519_pub?: string | null
}

export interface ModelOption {
  value: string
  name: string
}

/** One model provider on the serving node, from `/api/providers` and the `providers` stream event. */
export interface ProviderInfo {
  name: string
  state: 'connected' | 'pending' | 'failed' | 'disconnected'
  kind: 'api_key' | 'oauth' | null
  /** Whether the harness has a login flow for it; otherwise an API key is imported by CLI. */
  can_login: boolean
  url: string | null
  error: string | null
  identity: string | null
  expires_ms: number | null
  channels: string[]
  updated_ms: number | null
}

/** Hub reachability, from `/api/mesh` and the `mesh` stream event. */
export interface MeshState {
  hub: { state: 'disabled' } | { state: 'connected' } | { state: 'unreachable'; since_ms: number }
  hub_url: string | null
  node_id: string
  fingerprint: string | null
  last_ok_ms: number | null
  queued: number
  delivered_since_reconnect: number
  undecryptable: number
  last_error: string | null
  last_refusal: string | null
}

export interface Invite {
  code: string
  display_code: string
  url: string
  qr_svg: string | null
  channels: string[]
  expires_at: number
  state: 'waiting' | 'received' | 'admitted'
  received: { node_id: string; x25519_pub: string; name: string; contract: number; facts: string } | null
  received_fingerprint: string | null
  own_fingerprint: string | null
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
  node_id: string
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
  node_id: string
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
  node_id: string
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
  state: 'new' | 'claimed' | 'revising' | 'approved' | 'rejected' | 'gone'
  verdict_reason: string | null
  publish_result: string | null
  claimed_ms: number | null
  created_ms: number
}

export interface Queue {
  waiting: Permission[]
  reviews: Review[]
  /** Nightly memory-promotion batches, decided per item. */
  promotions: Promotion[]
  running: Session[]
  ended: Session[]
}

/** A promotion batch as the node keeps it; `items_json` holds the memories. */
export interface Promotion {
  id: string
  channel: string
  items_json: string
  state: 'open' | 'decided'
  verdicts_json: string | null
  decided_by: string | null
  decided_ms: number | null
  site: string
  hlc_ms: number
  created_ms: number
}

export interface PromotionItem {
  memory_id: string
  kind: 'fact' | 'lesson' | 'episode'
  scope: string
  scope_ref: string | null
  body: string
  confidence: number
  source_session: string | null
  source_node: string | null
  created_ms: number
}

export function promotionItems(p: Promotion): PromotionItem[] {
  try {
    return JSON.parse(p.items_json) as PromotionItem[]
  } catch {
    return []
  }
}

export type Frame =
  | ({ type: 'event' } & Event)
  | { type: 'chunk'; session_id: string; message_id: string | null; kind: string; text: string }
  | { type: 'tool_update'; session_id: string; tool_call_id: string; status: string | null }
  | ({ type: 'session' } & Session)
  | { type: 'queue'; waiting: Permission[] }
  | { type: 'reviews'; waiting: Review[] }
  | ({ type: 'node' } & NodeInfo)
  | ({ type: 'mesh' } & MeshState)
  | { type: 'providers'; providers: ProviderInfo[] }
  | { type: 'promotions'; waiting: Promotion[] }

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
