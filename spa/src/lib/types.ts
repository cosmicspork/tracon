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
  /** The channel this node prefers the composer to start on; only on the serving node's row. */
  default_channel?: string | null
}

export interface ModelOption {
  value: string
  name: string
}

export type LoginCompletion = 'local_callback' | 'paste' | 'device_code'

export interface ProviderConnectResult {
  url: string
  completion: LoginCompletion
  completion_note: string | null
  device_code?: string | null
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
  device_code?: string | null
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
  /** Of those, the frames still parked, waiting for the key that opens them. */
  held: number
  last_error: string | null
  last_refusal: string | null
}

/** An opt-in aggregate retained by a hub that holds this channel's key. */
export interface HubRollup {
  node_id: string
  seq: number
  captured_ms: number
  received_ms: number
  complete: boolean
  freshness: 'current' | 'stale'
  session_counts: Record<string, number>
  queued_permissions: number
  queued_reviews: number
  work_items: number
  documents: number
  memories: number
}

export interface HubRollups {
  channel: string
  state: 'current_complete' | 'partial' | 'stale'
  summaries: HubRollup[]
  coverage: {
    expected_nodes: string[]
    missing_nodes: string[]
    stale_nodes: string[]
    partial_nodes: string[]
    current_complete: boolean
  }
}

export interface TransferFile {
  path: string
  mode: number
  content_b64: string
}

/** A signed portable continuity package. Importing remains a separate explicit action. */
export interface CandidateTransfer {
  payload: {
    version: number
    candidate_id: string
    channel: string
    origin_node: string
    target_node: string | null
    created_ms: number
    candidate: unknown
    evidence: unknown
    files: TransferFile[]
    context: { documents: unknown[]; memories: unknown[]; note: string }
  }
  sha256: string
  signature: string
}

export interface TransferStage {
  id: string
  candidate_id: string
  channel: string
  origin_node: string
  target_node: string | null
  files: number
  documents: number
  memories: number
  confirmation_required: boolean
}

export interface TransferInboxItem {
  id: string
  candidate_id: string
  channel: string
  origin_node: string
  target_node: string | null
  created_ms: number
  files: number
  documents: number
  memories: number
  note: string
  import_state: 'preparing' | 'imported' | 'failed' | null
  session_id: string | null
}

export interface TransferImport {
  transfer_id: string
  workspace_id: string
  session: Session
  source_session_changed: false
}

export interface Invite {
  code: string
  display_code: string
  url: string
  qr_svg: string | null
  channels: string[]
  expires_at: number
  state: 'waiting' | 'received' | 'admitted'
  received: {
    node_id: string
    x25519_pub: string
    name: string
    contract: number
    facts: string
    binding_sig: string
  } | null
  received_fingerprint: string | null
  own_fingerprint: string | null
}

export type SessionState =
  | 'starting'
  | 'running'
  | 'paused'
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
  /** What the node expected of the harness: the version it is pinned to. */
  harness_version: string
  /** What the harness reported at the handshake, and the protocol revision
   * this session negotiated. Null until it starts, and on older rows. */
  harness_agent: string | null
  harness_found: string | null
  harness_protocol: string | null
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
  /** Archived as legacy: this session's harness is one the node no longer
   * has, so it is read-only for good. The row keeps `harness_id` and
   * `harness_version` so the transcript stays interpretable. */
  legacy_ms?: number | null
  /** Lineage, when this session was made from another one. */
  parent_session?: string | null
  continued_from?: string | null
  /** The launch manifest this session was staged from. Written once, at
   * launch: a later revision is for the next session, not this one. */
  manifest_digest?: string | null
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

/** What the session holding a request was started to do, joined on by the node.
 *  Every field is optional: a mirrored request can arrive before its session. */
export interface PermissionIntent {
  channel: string | null
  phase: string | null
  branch: string | null
  work_item_id: string | null
  work_item_title: string | null
  /** The state of the session that raised it; a request whose session ended
   *  can no longer reach a running harness, whatever is answered. */
  session_state: SessionState | null
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
  /** Absent on a row from a node that predates the join. */
  intent?: PermissionIntent | null
}

export interface OperatorQuestion {
  id: string
  session_id: string
  channel: string
  node_id: string
  prompt: string
  choices_json: string
  request_key?: string | null
  state: 'unanswered' | 'answered' | 'cancelled'
  answer_json: string | null
  created_ms: number
  answered_ms: number | null
}

export interface OperatorIssue {
  id: string
  session_id: string
  channel: string
  title: string
  body: string
  attachments_json: string
  state: 'draft' | 'publishing' | 'published' | 'uncertain'
  published_url: string | null
  publish_error: string | null
  created_ms: number
  approved_ms: number | null
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
  state: 'new' | 'claimed' | 'revising' | 'publishing' | 'approved' | 'acknowledged' | 'rejected' | 'gone'
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

export interface ReviewContext {
  path: string
  start_line: number
  end_line: number
  text: string
}

/** Immutable review evidence, distinct from the mutable review card. */
export interface CandidateEvidence {
  candidate: {
    id: string
    head_sha: string
    tree_sha: string | null
    channel: string
    owner_session_id: string
    source_kind: string
    captured_ms: number
    capture_json: string
  }
  checks: CandidateCheckRun[]
  revisions: {
    id: string
    review_id: string
    candidate_id: string
    title: string
    body: string
    diff: string
    files: string
    head_sha: string
    context_json: string
    requirements_work_item_id: string | null
    requirements_title: string | null
    requirements_body: string | null
    requirements_hash: string | null
    created_ms: number
  }[]
  decisions: {
    id: string
    review_id: string
    revision_id: string
    decision: string
    source: 'operator' | 'authority' | 'legacy_unknown'
    reason: string | null
    title: string | null
    body: string | null
    patch: string | null
    decided_ms: number
  }[]
  demonstrations: {
    id: string
    candidate_id: string
    channel: string
    document_id: string
    document_slug: string
    document_hash: string
    label: string
    created_ms: number
    /** The document has been edited or deleted since this was attached. */
    stale: boolean
  }[]
}

export interface CandidateCheckRun {
  id: string
  candidate_id: string | null
  session_id: string
  /** The operator-configured command that ran; null on a pre-migration row. */
  command: string | null
  definition_json: string
  definition_hash: string | null
  execution_image: string | null
  inputs_json: string | null
  reuse_key: string | null
  outcome: 'running' | 'passed' | 'failed' | 'interrupted' | 'cancelled' | 'reused'
  source_outcome: string | null
  exit_code: number | null
  log: string
  duration_ms: number | null
  started_ms: number
  finished_ms: number | null
  rerun_of: string | null
  reused_from_id: string | null
  metadata_json: string
}

export interface ReviewDetails {
  review: Review
  /**
   * What this screen is showing. A verdict names it, and the node refuses one
   * that names a revision the review has moved past — the commit alone cannot
   * say, because a resubmission may carry the same one with different
   * requirements or prose. Null for a narrative report, and for a legacy row
   * that has no revision recorded.
   */
  revision: ReviewRevisionRef | null
  stale: string[]
  /** Pinned to the revision at submit time; never the live work item. */
  requirements: PinnedRequirements | null
  surrounding_code: ReviewContext[]
  evidence: CandidateEvidence | null
  legacy_check_events: CandidateCheckRun[]
}

export interface ReviewRevisionRef {
  id: string
  head_sha: string
  created_ms: number
}

export interface PinnedRequirements {
  id: string
  title: string
  body: string
  hash: string
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
  /** The document holding this item's product brief, when it has one. */
  brief_slug: string | null
  closed_by_session: string | null
  created_ms: number
  updated_ms: number
}

/** Who is behind a brief line. An unmarked line stays `unattributed`. */
export type Provenance = 'observed' | 'inferred' | 'decided' | 'unattributed'

export type BriefField =
  | 'intended_user'
  | 'problem'
  | 'source_references'
  | 'constraints'
  | 'success_criteria'
  | 'unresolved_questions'

export type RefKind = 'doc' | 'session' | 'evidence' | 'work' | 'url' | 'file'

/** What a line points at. `known` is absent where the node cannot tell. */
export interface BriefRef {
  kind: RefKind
  value: string
  label?: string
  known?: boolean
}

export interface BriefEntry {
  provenance: Provenance
  text: string
  refs: BriefRef[]
}

export interface BriefSection {
  field: BriefField
  heading: string
  notes: string
  entries: BriefEntry[]
}

/** A brief as the node reads it out of its document. */
export interface Brief {
  channel: string
  slug: string
  work_item_id?: string
  hash: string
  updated_ms: number
  title: string
  preamble: string
  sections: BriefSection[]
  extra: string
  counts: { observed: number; inferred: number; decided: number; unattributed: number }
  absent: { field: BriefField; heading: string; says: string }[]
}

/** One line, as the interface sends it. */
export interface BriefEntryInput {
  provenance?: Provenance
  text: string
  refs: { kind: RefKind; value: string }[]
}

export type Blocker = { kind: 'open'; id: string } | { kind: 'unknown'; id: string } | { kind: 'cycle' }
export type Readiness = { state: 'ready' } | { state: 'blocked'; by: Blocker[] } | { state: 'closed' }

/** The ledger view: the item, its derived readiness, and the session holding it. */
export type WorkView = WorkItem & { readiness: Readiness; session_id: string | null }

/** One allow rule that would cover an action if its arguments were in scope. */
export interface ScopedAllow {
  rule_id: string
  reason: string
  args: Record<string, string[]>
}

/** Commands the bundle auto-approves, as the rule that approves them names them. */
export interface UnattendedCommands {
  rule_id: string
  reason: string
  commands: string[]
}

/** One named action and what the node would answer for it right now. */
export interface ActionStanding {
  name: string
  /** `tool`, `authority` or `capability`. */
  surface: string
  verdict: 'allow' | 'deny' | 'ask'
  rule_id: string | null
  reason: string | null
  /** Present on an `ask`: the narrower allows that would cover it. */
  scoped: ScopedAllow[]
  /** Authority actions only. */
  grants?: AuthorityGrant[]
}

/**
 * What a session may do, from `/api/sessions/{id}/authority`. Every verdict is
 * produced by the functions that decide the real call, so this is the gate
 * answering about itself rather than a description kept beside it.
 */
export interface SessionAuthority {
  session_id: string
  channel: string
  node: {
    id: string
    name: string | null
    is_self: boolean
    reachable: boolean
    isolation: string | null
    failed_check: string | null
    failed_detail: string | null
  }
  harness: { id: string; expected: string; found: string | null; agent: string | null; protocol: string | null }
  image: { runtime: string; execution: string; manifest_digest: string | null }
  access: {
    credentials: string[]
    egress: string[]
    workspace: string
    repo: string
    branch: string
    external_broker: boolean
  }
  limits: {
    budget_tokens: number
    tokens_used: number
    permission_timeout_secs: number
    ceiling: CeilingInfo
  }
  policy: { version: number; trusted: boolean; rules: number }
  actions: ActionStanding[]
  unattended_commands: UnattendedCommands[]
  grants: AuthorityGrant[]
}

export interface CeilingInfo {
  usage_today: number
  ceiling: number | null
  state: 'under' | 'near' | 'at' | 'none'
  /** Turns today the gateway could not count. They add nothing to
   *  `usage_today`, which is why they are shown next to it. */
  unmetered_turns: number
}

/** The verdict on one turn's two usage sources. */
export type UsageState = 'open' | 'reconciled' | 'mismatch' | 'unmetered'

/** One turn as both sources saw it. `charged_tokens` is what the budget paid:
 *  never less than the gateway counted on the wire. */
export interface TurnUsage {
  turn: number
  gateway_input: number
  gateway_output: number
  gateway_requests: number
  harness_tokens: number | null
  harness_cost_usd: number | null
  charged_tokens: number
  state: UsageState
  started_ms: number
  settled_ms: number | null
}

export interface SessionUsage {
  /** The last settled turn's verdict, or null before any turn has settled. */
  state: UsageState | null
  gateway_tokens: number
  harness_tokens: number
  charged_tokens: number
  unmetered_turns: number
  mismatched_turns: number
  turns: TurnUsage[]
}

/** One language server or formatter the harness image was asked for.
 *
 *  Three states, not five: OpenCode publishes no LSP status events and logs
 *  nothing when a server fails to spawn, so "starting", "running" and "failed"
 *  are not observable. What is observable is what the node named at launch and
 *  whether the image it launched from had it. */
export interface ToolStatus {
  id: string
  kind: 'lsp' | 'formatter'
  version: string
  path: string
  state: 'configured' | 'unavailable' | 'disabled'
}

export interface ToolchainStatus {
  revision: string
  /** What the image reported. Null when it carried no profile at all. */
  image_revision: string | null
  tools: ToolStatus[]
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
  setup_failures: number
  verified_sessions: number
  seconds_to_first_verified_candidate: number | null
  interventions: number
  question_wait_seconds: number
  human_wait_seconds: number
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

/** One bounded forge page. `complete` is false while another page remains or
 * the provider returned malformed pagination metadata. */
export interface ForgeList {
  forge: string
  repos: ForgeRepo[]
  next_cursor?: string
  complete: boolean
  error?: string
}

export interface PolicyRule {
  id: string
  verdict: 'allow' | 'ask' | 'deny'
  reason: string
  kinds: string[]
  channels: string[]
  matches: string[]
  args: Record<string, string[]>
}

/** A narrow local authority decision. It never edits the signed policy bundle. */
export interface AuthorityGrant {
  id: string
  action:
    | 'merge'
    | 'publish'
    | 'ticket_transition'
    | 'deploy'
    | 'browser_verify'
    | 'browser_test_account'
    /** An interactive terminal in one session's workspace. Bound to that session. */
    | 'terminal'
  verdict: 'allow' | 'ask' | 'deny'
  target: string
  channel: string
  session_id: string | null
  revision: string | null
  expires_ms: number | null
  revoked_ms: number | null
  reason: string
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

/** A document as the node keeps it. `body` is empty in listings. */
export interface Document {
  id: string
  channel: string
  slug: string
  kind: string
  title: string
  body: string
  hash: string
  format: 'markdown' | 'html'
  entry_path?: string | null
  source_name?: string | null
  bundle_files?: { path: string; media_type: string; size_bytes: number }[]
  site: string
  hlc_ms: number
  deleted: number
  /** 1 when archived: kept and readable, but out of listings and search. */
  archived?: number
  /** 1 when pinned: included in full in every session's orientation. */
  pinned?: number
  created_ms: number
  updated_ms: number
}

/** A memory as the node keeps it. */
export interface Memory {
  id: string
  channel: string
  scope: string
  scope_ref: string | null
  kind: 'directive' | 'fact' | 'lesson' | 'episode' | string
  body: string
  source_session: string | null
  source_node: string | null
  confidence: number
  state: 'candidate' | 'proposed' | 'active' | 'promoted' | 'rejected' | string
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
  format?: 'markdown' | 'html' | null
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

/** A verdict on one item: the bare legacy form, or an object carrying an
 * edited body alongside a promoted item's verdict. */
export type PromotionVerdict = 'promote' | 'reject' | { verdict: 'promote' | 'reject'; body?: string }

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
/** One model a node declares under a provider: what the picker offers and a declaring harness is told. */
export interface ModelDecl {
  id: string
  name?: string
  context?: number
  output?: number
  reasoning?: boolean
  attachment?: boolean
}

/** A provider entry as the settings pane sees it: the catalogue is writable, the rest is context. */
export interface ProviderConfig {
  shape: string
  upstream: string
  credential: string
  login: string | null
  models: ModelDecl[]
}

export interface NodeConfig {
  node_name: string
  harness: { id: string; version: string; tools: string[] }
  session: { budget_tokens: number; permission_timeout_secs: number; default_channel: string }
  review: { max_diff_lines: number; max_files: number }
  gateway: { allow_hosts: string[] }
  publish: { gh: string; glab: string; git: string }
  boundary: { podman: string }
  external: { enabled: boolean; idle_timeout_secs: number }
  launch: { plugins: string[] }
  providers: Record<string, ProviderConfig>
  readonly: { hub_url: string | null; runtime: string; config_path: string }
  running: { harness_id: string; harness_version: string; node_name: string }
}

/** One thing an operator put in a channel's launch manifest. */
export interface ManifestItem {
  channel: string
  kind: 'skill' | 'instruction' | 'agent'
  name: string
  source: string
  digest: string
  body: string
  warnings: string[]
  imported_ms: number
}

/** A built manifest: what a launch on this channel would stage. */
export interface LaunchManifest {
  channel: string
  revision: number
  digest: string
  skills: { name: string; description: string; source: string; digest: string }[]
  instructions: { name: string; body: string }[]
  agents: { name: string; body: string }[]
  plugins: string[]
  lsp: { name: string; command: string[] }[]
  formatters: { name: string; command: string[] }[]
  providers: string[]
  policy_revision: string
}

export interface ManifestView {
  channel: string
  items: ManifestItem[]
  /** The latest revision this node recorded, which running sessions may hold. */
  recorded: LaunchManifest | null
  /** What the next launch would build. Null when it would be refused. */
  next: LaunchManifest | null
  /** Why it would be refused, when it would. */
  error: string | null
  /** Plugin packages the harness image bakes; nothing else may be approved. */
  baked_plugins: string[]
}

export interface EnrollStatus {
  lines: string[]
  done: boolean
  error: string | null
  channels: string[]
  restart_required: boolean
}

/** Candidate-bound, node-authoritative QA deployment observation. */
export interface QaDeployment {
  id: string
  candidate_id: string
  channel: string
  target_id: string
  build_id: string
  execution_image: string
  origin: string
  environment_identity: string | null
  identity_state: 'fresh' | 'unknown' | 'failed'
  observed_ms: number
  started_ms: number
  finished_ms: number
  outcome: 'succeeded' | 'failed' | 'unknown'
  detail_json: string
}

export interface BrowserAssertionResult {
  kind: string
  ok: boolean
  detail: string
}

export interface QaBrowserRun {
  id: string
  deployment_id: string
  candidate_id: string
  channel: string
  target_id: string
  authorized_origins_json: string
  test_credential: string | null
  assertions_json: string
  outcome: 'passed' | 'failed' | 'unknown'
  environment_before: string | null
  environment_after: string | null
  evidence_state: 'fresh' | 'stale' | 'unknown'
  log_tail: string
  started_ms: number
  finished_ms: number
}

export interface QaAsset {
  id: string
  browser_run_id: string
  candidate_id: string
  channel: string
  kind: 'screenshots' | 'browser-log' | 'demonstration'
  document_id: string
  document_hash: string
  slug: string
  created_ms: number
}

export interface Prototype {
  id: string
  candidate_id: string
  channel: string
  source_revision: string
  source_identity_json: string
  build_image: string
  build_inputs_json: string
  document_id: string | null
  document_hash: string | null
  slug: string
  entry_path: string
  outcome: 'succeeded' | 'failed' | 'unknown'
  detail: string
  created_ms: number
  finished_ms: number
}

export interface QaTarget {
  id: string
  /** `gitlab` plays a manual pipeline job; `command` runs a brokered argv on the node. */
  kind: 'gitlab' | 'command'
  /** A discovery target reports the attested host suffix (`*.example.com`) instead. */
  origin: string
  /** A discovery target reports its identity path, which hangs off whatever origin it finds. */
  identity_url: string
  identity_header: string
  /** Empty for a command target: its equivalent is a digest, and it exists only per deployment. */
  execution_image: string
  /** Command targets only: the configured argv, before placeholder substitution. */
  deploy_command: string[]
  /** Command targets only: the broker entry whose environment the command receives. */
  env_credential: string
  browser_image: string
  test_credential: string | null
  missing_grants: string[]
}

export interface QaEvidence {
  candidate: {
    id: string
    channel: string
    head_sha: string
    owner_session_id: string
    /** The node that captured every deployment and browser-proof row below. */
    owner_node_id: string
    captured_ms: number
  }
  targets: QaTarget[]
  deployments: QaDeployment[]
  browser_runs: QaBrowserRun[]
  assets: QaAsset[]
  prototypes: Prototype[]
}
