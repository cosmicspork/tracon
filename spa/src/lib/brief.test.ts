import { expect, test } from 'bun:test'
import { entriesOf, nothingObserved, parseRefs, refHref, refLabel, restsOn } from './brief'
import type { Brief } from './types'

function brief(partial: Partial<Brief> = {}): Brief {
  return {
    channel: 'personal',
    slug: 'brief-abc123',
    hash: 'h',
    updated_ms: 0,
    title: 'Overnight alert triage',
    preamble: '',
    sections: [],
    extra: '',
    counts: { observed: 0, inferred: 0, decided: 0, unattributed: 0 },
    absent: [],
    ...partial,
  }
}

test('a reference this node has never seen says so where it is read', () => {
  expect(refLabel({ kind: 'doc', value: 'meeting-ops', label: 'Ride-along notes' })).toBe('Ride-along notes')
  expect(refLabel({ kind: 'doc', value: 'meeting-ops', known: false })).toBe('meeting-ops · not on this node')
  // A URL is not the node's to vouch for, so it is not reported as a gap.
  expect(refLabel({ kind: 'url', value: 'https://example.test/4821' })).toBe('https://example.test/4821')
})

test('references lead where the interface can follow them', () => {
  expect(refHref('personal', { kind: 'doc', value: 'meeting-ops' })).toBe('/docs/personal/meeting-ops')
  expect(refHref('personal', { kind: 'work', value: 'abc' })).toBe('/work/abc')
  expect(refHref('personal', { kind: 'session', value: 's1' })).toBe('/sessions/s1')
  expect(refHref('personal', { kind: 'url', value: 'https://example.test' })).toBe('https://example.test')
  expect(refHref('personal', { kind: 'file', value: 'src/main.rs' })).toBeNull()
  expect(refHref('personal', { kind: 'evidence', value: 'e1' })).toBeNull()
})

test('what was typed into the references box is read, and what cannot be is handed back', () => {
  const { refs, rejected } = parseRefs('doc:meeting-ops https://example.test/4821 ticket:4821 doc:')
  expect(refs).toEqual([
    { kind: 'doc', value: 'meeting-ops' },
    { kind: 'url', value: 'https://example.test/4821' },
  ])
  expect(rejected).toEqual(['ticket:4821', 'doc:'])
})

test('a brief says what it rests on, including what it does not say', () => {
  const b = brief({
    counts: { observed: 2, inferred: 3, decided: 1, unattributed: 1 },
    absent: [{ field: 'success_criteria', heading: 'Success criteria', says: 'no success criteria stated' }],
  })
  expect(restsOn(b)).toBe('2 observed · 3 inferred · 1 decided · 1 unattributed · no success criteria stated')
  expect(nothingObserved(b)).toBe(false)
})

test('a brief with nothing observed is one the customer has not been heard in', () => {
  expect(nothingObserved(brief({ counts: { observed: 0, inferred: 4, decided: 2, unattributed: 0 } }))).toBe(true)
  // An empty brief is not making a claim either way.
  expect(nothingObserved(brief())).toBe(false)
})

test('a section with no lines reads as no lines, not as missing', () => {
  const b = brief({
    sections: [
      {
        field: 'problem',
        heading: 'Problem',
        notes: '',
        entries: [{ provenance: 'inferred', text: 'arrival order buries the urgent alert', refs: [] }],
      },
    ],
  })
  expect(entriesOf(b, 'problem').length).toBe(1)
  expect(entriesOf(b, 'constraints')).toEqual([])
})
