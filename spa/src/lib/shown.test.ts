import { expect, test } from 'bun:test'

import { formatTag, shownEventLine, splitShown } from './shown'
import type { ShownWork } from './types'

function item(id: string, proposal?: boolean): ShownWork {
  return {
    id,
    channel: 'c',
    session_id: 's',
    lane: null,
    review_id: null,
    head_sha: 'bbbbbbbbbbbbbbbb',
    title: id,
    format: 'html',
    markdown: '',
    document_id: null,
    document_slug: null,
    document_hash: null,
    files: [],
    created_ms: 1,
    stale: false,
    stale_reason: null,
    checks: [],
    ...(proposal === undefined ? {} : { proposal }),
  }
}

test('proposals are read apart from accounts of work done, in the order shown', () => {
  const { work, proposals } = splitShown([item('a', false), item('p1', true), item('b'), item('p2', true)])
  expect(work.map((i) => i.id)).toEqual(['a', 'b'])
  expect(proposals.map((i) => i.id)).toEqual(['p1', 'p2'])
})

test('an item from a node before proposals is an account of work done', () => {
  expect(splitShown([item('old')]).proposals).toEqual([])
})

test('each format is tagged as what it is', () => {
  expect(formatTag({ format: 'markdown' })).toBe('account')
  expect(formatTag({ format: 'html' })).toBe('page')
  expect(formatTag({ format: 'files' })).toBe('files')
})

test("the log says a proposal was shown, not work", () => {
  expect(shownEventLine({ title: 'Two layouts', head_sha: 'abcdef0123456789', files: 1, proposal: true })).toBe(
    'proposal shown · Two layouts · at abcdef012345 · 1 file · in tracon only',
  )
  expect(shownEventLine({ title: 'Done', head_sha: 'abcdef0123456789', files: 0 })).toBe(
    'work shown · Done · at abcdef012345 · in tracon only',
  )
})
