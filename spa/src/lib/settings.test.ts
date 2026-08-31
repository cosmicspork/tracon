import { expect, test } from 'bun:test'
import { changedSubset, hashToken, loginUrl, mintToken } from './settings'

test('a minted token is the shape the node and the CLI both expect', () => {
  const token = mintToken(new Uint8Array(32).fill(0))
  expect(token.startsWith('trc1.')).toBe(true)
  // 32 bytes, base64url, unpadded.
  expect(token.slice(5)).toMatch(/^[A-Za-z0-9_-]{43}$/)
  // And two mints do not collide.
  expect(mintToken()).not.toBe(mintToken())
})

test('the node is told a hash and never the token', async () => {
  // The known SHA-256 of "abc", so this pins the algorithm and the encoding
  // rather than merely its own output.
  expect(await hashToken('abc')).toBe(
    'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad',
  )
  const hash = await hashToken(mintToken())
  expect(hash).toMatch(/^[0-9a-f]{64}$/)
})

test('the login url carries the token in the fragment', () => {
  const url = loginUrl('https://node.example.com/', 'trc1.abc')
  expect(url).toBe('https://node.example.com/#token=trc1.abc')
  // A fragment is never sent to a server, which is the whole point.
  expect(new URL(url).search).toBe('')
})

test('a patch carries only what changed', () => {
  const original = {
    node_name: 'nodeA',
    harness: { id: 'omp', version: '18.0.4' },
    gateway: { allow_hosts: ['^a$'] },
  }
  expect(changedSubset(original, structuredClone(original))).toEqual({})

  expect(
    changedSubset(original, {
      ...structuredClone(original),
      harness: { id: 'claude', version: '18.0.4' },
    }),
  ).toEqual({ harness: { id: 'claude' } })

  // A list is replaced wholesale, not merged.
  expect(
    changedSubset(original, { ...structuredClone(original), gateway: { allow_hosts: ['^b$'] } }),
  ).toEqual({ gateway: { allow_hosts: ['^b$'] } })
})
