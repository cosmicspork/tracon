import { call } from './api'
import type { PolicyRule } from './types'

export interface SignedPolicyBundle {
  toml: string
  signature: string
  trust_identity: string
  bundle_sha256: string
  policy: { version: number; rules: PolicyRule[]; trusted: boolean }
}

export interface PolicyInstallation {
  bundle_sha256: string
  source_node: string | null
  rollout_id: string | null
  applied_ms: number
}

export interface PolicyRolloutTarget {
  node_id: string
  /** pending/offline/sent/applied/rejected; sent is not an install confirmation. */
  status: string
  /** applied, rejected, or unconfirmed. */
  confirmation: 'applied' | 'rejected' | 'unconfirmed'
  /** receipt_capable only after an authenticated receipt; otherwise unknown_or_legacy. */
  compatibility: 'receipt_capable' | 'unknown_or_legacy'
  detail: string | null
  attempts: number
  last_sent_ms: number | null
  acknowledged_ms: number | null
}

export interface PolicyRollout {
  id: string
  bundle_sha256: string
  policy_version: number
  source_node: string
  created_ms: number
  updated_ms: number
  targets: PolicyRolloutTarget[]
}

export interface RunningPolicy {
  version: number
  rule_count: number
  trusted: boolean
}

export interface PolicyStatus {
  /** Independently verified signed files presently installed on disk. */
  installed: SignedPolicyBundle | null
  installed_error: string | null
  /** Policy the current node process uses to decide requests. */
  running: RunningPolicy
  trust_identity: string | null
  installation: PolicyInstallation | null
  rollouts: PolicyRollout[]
  signing_key_present: boolean
}

export const policyAdmin = {
  status: () => call<PolicyStatus>('GET', '/api/admin/policy'),
  initialize: () => call<SignedPolicyBundle>('POST', '/api/admin/policy/initialize', { confirm: true }),
  preview: (toml: string) => call<SignedPolicyBundle>('POST', '/api/admin/policy/preview', { toml }),
  apply: (bundle: SignedPolicyBundle, expected_sha256: string | null, targets: string[]) =>
    call<{ installed: SignedPolicyBundle; rollout: PolicyRollout | null }>('POST', '/api/admin/policy/apply', {
      toml: bundle.toml,
      signature: bundle.signature,
      bundle_sha256: bundle.bundle_sha256,
      expected_sha256,
      targets,
    }),
  retry: (id: string) => call<PolicyRollout>('POST', `/api/admin/policy/rollouts/${encodeURIComponent(id)}/retry`),
}
