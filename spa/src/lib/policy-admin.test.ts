import { describe, expect, test } from 'bun:test'
import { newerShipped, type PolicyStatus } from './policy-admin'

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
