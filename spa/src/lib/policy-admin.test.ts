import { describe, expect, test } from 'bun:test'
import { newerShipped, retryable, targetLabel, type PolicyRollout, type PolicyRolloutTarget, type PolicyStatus } from './policy-admin'

function status(installed: number | null, shipped: number | undefined): PolicyStatus {
  return {
    installed: installed === null
      ? null
      : {
          toml: '',
          signature: '',
          trust_identity: '',
          bundle_sha256: '',
          policy: { version: installed, rules: [], trusted: true },
        },
    installed_error: null,
    running: { version: installed ?? 0, rule_count: 0, trusted: installed !== null },
    trust_identity: null,
    installation: null,
    rollouts: [],
    signing_key_present: true,
    shipped: shipped === undefined ? undefined : { version: shipped, toml: 'version = 1' },
  }
}

describe('newerShipped', () => {
  test('names a shipped bundle newer than the installed one', () => {
    expect(newerShipped(status(17, 18))).toBe(18)
  })
  test('says nothing when the installed bundle is current or ahead', () => {
    expect(newerShipped(status(18, 18))).toBeNull()
    expect(newerShipped(status(19, 18))).toBeNull()
  })
  test('leaves a node with nothing installed to setup, and an older build alone', () => {
    expect(newerShipped(status(null, 18))).toBeNull()
    expect(newerShipped(status(17, undefined))).toBeNull()
  })
})

function target(status: string, confirmation: PolicyRolloutTarget['confirmation'] = 'unconfirmed'): PolicyRolloutTarget {
  return {
    node_id: 'n',
    status,
    confirmation,
    compatibility: 'unknown_or_legacy',
    detail: null,
    attempts: 0,
    last_sent_ms: null,
    acknowledged_ms: null,
  }
}

function rollout(...targets: PolicyRolloutTarget[]): PolicyRollout {
  return { id: 'r', bundle_sha256: 'h', policy_version: 21, source_node: 's', created_ms: 0, updated_ms: 0, targets }
}

describe('targetLabel', () => {
  test('says queued is not installed, and unqueued will be retried', () => {
    expect(targetLabel(target('sent'))).toBe('Queued at hub, unconfirmed')
    expect(targetLabel(target('offline'))).toBe('Not queued, will retry')
  })
  test('names a settled target by its outcome', () => {
    expect(targetLabel(target('applied', 'applied'))).toBe('Applied')
    expect(targetLabel(target('rejected', 'rejected'))).toBe('Rejected')
    expect(targetLabel(target('superseded', 'superseded'))).toBe('Superseded')
  })
})

describe('retryable', () => {
  test('offers Retry while a target is still unconfirmed', () => {
    expect(retryable(rollout(target('applied', 'applied'), target('offline')))).toBe(true)
    expect(retryable(rollout(target('sent')))).toBe(true)
  })
  test('hides Retry once every target is applied, rejected, or superseded', () => {
    expect(retryable(rollout(
      target('applied', 'applied'),
      target('rejected', 'rejected'),
      target('superseded', 'superseded'),
    ))).toBe(false)
  })
})
