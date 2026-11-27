import { expect, test } from 'bun:test'
import { droppedNote, proposedMessage, shipLine, shipping, shippedMessage } from './shipping'
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

test("with no message written the agent's own commit ships; a written different message drops its signature", () => {
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

// The plain line On the forge says about what ships (#1194), one per case.
const kept: ReviewIntent = {
  forge: {},
  commits: [
    { sha: 'f227bdd2aaaa', subject: 'one' },
    { sha: 'c5f35af5bbbb', subject: 'two' },
  ],
}
const line = (intent: ReviewIntent, edited: Record<string, string> = {}) =>
  shipLine(intent, shipping(intent, shippedMessage({}, intent), edited), edited)

test('two kept commits say two commits ship, not only the head', () => {
  expect(line(kept)).toBe('pushes 2 commits as written')
  expect(line({ ...kept, signed: ['f227bdd2aaaa', 'c5f35af5bbbb'] })).toBe('pushes 2 commits as written · signatures kept')
  expect(line({ ...kept, commits: kept.commits?.slice(1), signed: ['c5f35af5bbbb'] })).toBe(
    'pushes 1 commit as written · signature kept',
  )
})

test('when only some kept commits are signed, the line names whose signature is kept', () => {
  expect(line({ ...kept, signed: ['c5f35af5bbbb'] })).toBe('pushes 2 commits as written · signature on c5f35af5 kept')
})

test('reworded commits count the messages edited here and say they lose their signatures', () => {
  const signed = { ...kept, signed: ['c5f35af5bbbb'] }
  expect(line(signed, { f227bdd2aaaa: 'one, said again' })).toBe(
    'pushes 2 commits · 1 message edited here · reworded commits lose their signatures',
  )
  expect(line(kept, { f227bdd2aaaa: 'a', c5f35af5bbbb: 'b' })).toBe('pushes 2 commits · 2 messages edited here')
})

test('a squash pushes one commit of the reviewed tree, the head itself when the message is its own', () => {
  expect(shipLine(one, shipping(one, 'something else', {}), {})).toBe('pushes one commit of the reviewed tree')
  expect(shipLine(one, shipping(one, 'work', {}), {})).toBe(
    "pushes one commit of the reviewed tree · the agent's own aaaa1111, signature kept",
  )
})

test('dropped signatures are listed apart, and only when a new commit drops them', () => {
  expect(droppedNote({ own: true, dropped: [] })).toBe('')
  expect(droppedNote({ own: false, dropped: ['aaaa1111'] })).toBe(
    'The signature on aaaa1111 is dropped: a commit the node makes is never signed.',
  )
  expect(droppedNote({ own: false, dropped: ['aaaa1111', 'bbbb2222'] })).toBe(
    'The signatures on aaaa1111 and bbbb2222 are dropped: a commit the node makes is never signed.',
  )
})
