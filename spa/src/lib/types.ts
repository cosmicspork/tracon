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
  /** The node's provider summary, carried in its hello. Absent from older builds. */
  providers?: ProviderInfo[] | null
  /**
   * Whether *this client* reached the node over loopback, and so may change
   * what the node is rather than only what it does. Only ever set on the
   * serving node's own row, by the request that asked.
   */
  loopback?: boolean
}

export interface ModelOption {
  value: string
  name: string
}

export type LoginCompletion = 'local_callback' | 'paste'

export interface ProviderConnectResult {
  url: string
  completion: LoginCompletion
  completion_note: string | null
}

/** One model provider on the serving node, from `/api/providers` and the `providers` stream event. */
export interface ProviderInfo {
  name: string
  state: 'connected' | 'pending' | 'failed' | 'disconnected'
  kind: 'api_key' | 'oauth' | null
  /** Whether the harness has a login flow for it; otherwise an API key is imported in Settings. */
  can_login: boolean
  /** Private to the serving node; peer summaries deliberately omit completion details. */
  url?: string | null
  completion?: LoginCompletion | null
  completion_note?: string | null
  error?: string | null
  identity: string | null
  expires_ms: number | null
  channels: string[]
  updated_ms: number | null
}

/** What the broker will say about a credential: bindings and key names, never a value. */
export interface CredentialSummary {
  name: string
  kind: string
  provider: string | null
  channels: string[]
  nodes: string[]
  identity: string | null
  expires_ms: number | null
  env_keys: string[]
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
  phase: 'plan' | 'execute' | 'review'
  policy_version: number | null
  /** Review sessions: the review this session was spawned to read. */
  review_id: string | null
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
  /** Put away: kept in full, just not listed on the home. */
  archived_ms?: number | null
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
  /** The deterministic checks that passed at submit, as JSON. */
  checks_json: string | null
  /** The fresh session the node spawned to read this review, if any. */
  review_session_id: string | null
  /** That session's verdict, as JSON: `{verdict, summary, findings, model}`. */
  ai_verdict_json: string | null
  /** A diff the operator edited by hand, carried back with the notes. */
  revision_patch?: string | null
}

export interface CheckResult {
  command: string
  ok: boolean
  exit: number | null
  tail: string
  ms: number
}

export interface AiVerdict {
  verdict: 'approve' | 'request_changes'
  summary: string
  findings: { path?: string; line?: number; severity?: 'blocking' | 'should' | 'nit'; note: string }[]
  model: string
  session_id: string
  at_ms: number
}

export function reviewChecks(r: Review): CheckResult[] {
  try {
    return r.checks_json ? (JSON.parse(r.checks_json) as CheckResult[]) : []
  } catch {
    return []
  }
}

export function reviewVerdict(r: Review): AiVerdict | null {
  try {
    return r.ai_verdict_json ? (JSON.parse(r.ai_verdict_json) as AiVerdict) : null
  } catch {
    return null
  }
}

/** A work item as the node keeps it; `deps` are the ids it waits on. */
export interface WorkItem {
  id: string
  channel: string
  project_id: string | null
  title: string
  body: string
  state: 'open' | 'closed'
  priority: number
  deps: string[]
  discovered_from: string | null
  discovered_by_session: string | null
  phase_plan_slug: string | null
  closed_by_session: string | null
  created_ms: number
  updated_ms: number
}

export type Blocker = { kind: 'open'; id: string } | { kind: 'unknown'; id: string } | { kind: 'cycle' }
export type Readiness = { state: 'ready' } | { state: 'blocked'; by: Blocker[] } | { state: 'closed' }

/** The ledger view: the item, its derived readiness, and the session holding it. */
export type WorkView = WorkItem & { readiness: Readiness; session_id: string | null }

export interface CeilingInfo {
  usage_today: number
  ceiling: number | null
  state: 'under' | 'near' | 'at' | 'none'
}

/** A browser this node pushes to, from `/api/push/subscriptions`. */
export interface PushDevice {
  id: string
  user_agent: string | null
  created_ms: number
  last_ok_ms: number | null
  fail_count: number
  /** A browser on the node's own machine, which never logs in. */
  local: boolean
  /** Registered by this browser's session. */
  mine: boolean
}

export interface PhaseBinding {
  model?: string
  budget_tokens?: number
  requires_plan?: boolean
}

/** Free-form on the wire; these are the keys the node and the interface read. */
export interface ChannelBindings {
  phases?: Record<string, PhaseBinding>
  ceiling_tokens_per_day?: number
  [key: string]: unknown
}

export interface ChannelInfo {
  name: string
  nodes: string[]
  bindings: ChannelBindings
  ceiling: CeilingInfo
  /** Set when the channel is archived: it keeps its work and takes no new sessions. */
  archived?: number | null
}

export interface ChannelMetrics {
  channel: string
  since_ms: number
  accepted_changes: number
  rejected_changes: number
  approvals: number
  approvals_per_accepted_change: number | null
  tokens_per_accepted_change: number | null
  tokens: number
  cost_usd: number | null
  human_seconds: number
  agent_seconds: number
  sessions: number
}

/** One repository this node has run sessions against. */
export interface RecentRepo {
  repo_path: string
  last_used_ms: number
  sessions: number
}

/** A clone the node manages under its own state, ready before any session. */
export interface ManagedRepo {
  repo_path: string
  full_name: string
  host: string
}

/** One repository as a forge lists it. */
export interface ForgeRepo {
  host: string
  owner: string
  name: string
  full_name: string
  private: boolean
  default_branch: string | null
  pushed_at: string | null
}

/** One forge's listing: repositories, or why there are none to show. */
export interface ForgeList {
  forge: string
  repos: ForgeRepo[]
  error?: string
}

export interface Queue {
  waiting: Permission[]
  reviews: Review[]
  /** Nightly memory-promotion batches, decided per item. */
  promotions: Promotion[]
  running: Session[]
  ended: Session[]
}

/** A document as the node keeps it. `body` is empty in listings. */
export interface Document {
  id: string
  channel: string
  slug: string
  kind: string
  title: string
  body: string
  hash: string
  site: string
  hlc_ms: number
  deleted: number
  created_ms: number
  updated_ms: number
}

/** A search hit across memories and documents. */
export interface RecallHit {
  kind: string
  id: string
  slug: string | null
  title: string | null
  text: string
  scope: string | null
  confidence: number | null
  rank: number
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
  | { type: 'changes'; channel: string; changes: { table: string; id: string; op: string }[] }

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

/** One boundary check, as the node reports it. */
export interface BoundaryCheck {
  id: string
  ok: boolean
  detail: string
}

export interface BoundaryResult {
  state: string
  checks: { checks: BoundaryCheck[] }
}

/** The settings the interface writes, and the context it needs to explain them. */
export interface NodeConfig {
  node_name: string
  harness: { id: string; version: string; tools: string[] }
  session: { budget_tokens: number; permission_timeout_secs: number }
  review: { max_diff_lines: number; max_files: number }
  gateway: { allow_hosts: string[] }
  publish: { gh: string; glab: string; git: string }
  boundary: { podman: string }
  readonly: { hub_url: string | null; runtime: string; config_path: string }
  running: { harness_id: string; harness_version: string; node_name: string }
}

export interface EnrollStatus {
  lines: string[]
  done: boolean
  error: string | null
  channels: string[]
  restart_required: boolean
}
