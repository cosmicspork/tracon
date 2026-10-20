import { expect, test } from 'bun:test'
import { proposedMessage, shipping, shippedMessage } from './shipping'
import type { ReviewIntent } from './types'

const one: ReviewIntent = {
  forge: {},
  squash_onto: 'base',
  commits: [{ sha: 'aaaa1111', subject: 'work' }],
  as_written: 'work',
  signed: ['aaaa1111'],
}

test("the agent's message ships its own signed commit", () => {
  expect(shipping(one, 'work\n', {})).toEqual({ own: true, dropped: [] })
})

test('an edited message ships a new commit and says the signature is dropped', () => {
  expect(shipping(one, 'work, said again', {})).toEqual({ own: false, dropped: ['aaaa1111'] })
})

test('two commits squash to a new commit, dropping the signatures they carry', () => {
  const two: ReviewIntent = {
    forge: {},
    squash_onto: 'base',
    commits: [
      { sha: 'aaaa1111', subject: 'work' },
      { sha: 'bbbb2222', subject: 'more' },
    ],
    signed: ['bbbb2222'],
  }
  expect(shipping(two, 'more', {})).toEqual({ own: false, dropped: ['bbbb2222'] })
})

test('an unsigned commit remade drops nothing', () => {
  expect(shipping({ ...one, signed: undefined }, 'other', {})).toEqual({ own: false, dropped: [] })
})

test('under keep, an edited commit and the ones after it are remade', () => {
  const keep: ReviewIntent = {
    forge: {},
    commits: [
      { sha: 'aaaa1111', subject: 'one' },
      { sha: 'bbbb2222', subject: 'two' },
      { sha: 'cccc3333', subject: 'three' },
    ],
    signed: ['aaaa1111', 'cccc3333'],
  }
  expect(shipping(keep, '', {})).toEqual({ own: true, dropped: [] })
  expect(shipping(keep, '', { bbbb2222: 'two, said again' })).toEqual({ own: false, dropped: ['cccc3333'] })
})

test("with no message written, the agent's own commit ships whatever the title says", () => {
  const titled: ReviewIntent = { ...one, as_written: 'work\n\nthe detail', own_message: 'work\n\nthe detail' }
  expect(shipping(titled, shippedMessage({}, titled), {})).toEqual({ own: true, dropped: [] })
  expect(shipping(titled, shippedMessage({ commit: 'work, retitled' }, titled), {})).toEqual({
    own: false,
    dropped: ['aaaa1111'],
  })
})

test("the squash message is the one written, else the agent's own, never the description", () => {
  const description = { title: 'described', body: '## Why' }
  const own: ReviewIntent = { forge: { description }, squash_onto: 'base', own_message: 'fix: own' }
  expect(shippedMessage({ commit: ' written\n', description }, own)).toBe('written')
  expect(shippedMessage({ description }, own)).toBe('fix: own')
  expect(proposedMessage(own)).toBe('fix: own')
  expect(proposedMessage({ ...own, forge: { description, commit: 'fix: agent' } })).toBe('fix: agent')
  expect(proposedMessage({ forge: { description }, squash_onto: 'base' })).toBe('')
})
