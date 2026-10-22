import { expect, test } from 'bun:test'
import {
  CLOSE_TAG,
  FROM_AI_REVIEW,
  findingLine,
  hasAiBlock,
  markedFromAi,
  openAiRequest,
  openTag,
  requestText,
  sentReason,
  withFinding,
} from './reviewerChanges'
import type { AiFinding } from './types'

const findings: AiFinding[] = [
  { path: 'a.txt', line: 2, severity: 'should', note: 'cover it with a test' },
  { severity: 'nit', note: 'the summary line is long' },
  { path: 'src/lib.rs', severity: 'blocking', note: 'this panics on empty input\nreturn an error instead' },
]
const request = (summary: string, list: AiFinding[] = findings, at = 1) => ({ summary, findings: list, at })

test('each finding is one item, with its severity and where it is', () => {
  expect(findingLine(findings[0])).toBe('- [should] a.txt:2 — cover it with a test')
  expect(findingLine({ path: 'b.ts', note: 'x' })).toBe('- [should] b.ts — x')
  expect(findingLine({ line: 9, severity: 'nit', note: 'x' })).toBe('- [nit] line 9 — x')
  expect(findingLine(findings[1])).toBe('- [nit] the summary line is long')
})

test('a note of several lines stays under its item', () => {
  expect(findingLine(findings[2])).toBe('- [blocking] src/lib.rs — this panics on empty input\n  return an error instead')
})

test('a finding that is not what its fields say still makes a line', () => {
  expect(findingLine({ path: 'a.ts' } as unknown as AiFinding)).toBe('- [should] a.ts — ')
  expect(findingLine({ note: 42, line: 'two' } as unknown as AiFinding)).toBe('- [should] 42')
  expect(() => openAiRequest('', request('s', [{ severity: 'nit' } as unknown as AiFinding]))).not.toThrow()
})

test('the composer opens on the summary and every finding, marked and tagged with the verdict', () => {
  expect(requestText(request('a.txt grew without a test', findings, 7))).toBe(
    [
      openTag(7),
      FROM_AI_REVIEW,
      '',
      'a.txt grew without a test',
      '',
      '- [should] a.txt:2 — cover it with a test',
      '- [nit] the summary line is long',
      '- [blocking] src/lib.rs — this panics on empty input',
      '  return an error instead',
      CLOSE_TAG,
    ].join('\n'),
  )
  expect(requestText(request('only a summary', []))).toBe(`${openTag(1)}\n${FROM_AI_REVIEW}\n\nonly a summary\n${CLOSE_TAG}`)
})

test('unticking a finding takes it out of the text and ticking puts it back in place', () => {
  const text = requestText(request('sum'))
  const out = withFinding(text, findings[1], false, findings)
  expect(out).not.toContain('the summary line is long')
  expect(out).toContain('a.txt:2')
  expect(withFinding(out, findings[1], true, findings)).toBe(text)
  expect(withFinding(text, findings[2], false, findings)).not.toContain('return an error instead')
  // The first goes back before the rest, and with none left, inside the block.
  expect(withFinding(withFinding(text, findings[0], false, findings), findings[0], true, findings)).toBe(text)
  const bare = findings.reduce((t, f) => withFinding(t, f, false, findings), text)
  expect(withFinding(bare, findings[1], true, findings).endsWith(`- [nit] the summary line is long\n${CLOSE_TAG}`)).toBe(
    true,
  )
})

test('a finding whose item starts another one is unticked without cutting into it', () => {
  const short = { path: 'a.ts', note: 'x' }
  const long = { path: 'a.ts', note: 'x and y' }
  const multi = { path: 'a.ts', note: 'x\nthen y' }
  const all = [short, long, multi]
  const text = requestText(request('sum', all))
  const out = withFinding(text, short, false, all)
  expect(out).toContain('- [should] a.ts — x and y')
  expect(out).toContain('- [should] a.ts — x\n  then y')
  expect(out.split('\n').filter((l) => l === '- [should] a.ts — x').length).toBe(1)
  expect(withFinding(out, short, true, all)).toBe(text)
  const noMulti = withFinding(text, multi, false, all)
  expect(noMulti).not.toContain('then y')
  expect(noMulti.split('\n').filter((l) => l === '- [should] a.ts — x').length).toBe(1)
})

test('opening the request keeps what was written, after the text from the reviewer', () => {
  const opened = openAiRequest('and rename the flag', request('sum'))
  expect(opened.text).toBe(`${requestText(request('sum'))}\n\nand rename the flag`)
  expect(opened.ticked).toEqual([true, true, true])
})

test('opening it again on the same verdict does not add its text twice', () => {
  const first = openAiRequest('', request('sum'))
  const unticked = withFinding(first.text, findings[1], false, findings)
  const again = openAiRequest(unticked, request('sum'))
  expect(again.text).toBe(unticked)
  expect(again.ticked).toEqual([true, false, true])
})

test('a draft restored after a reload has its earlier verdict replaced by the new one', () => {
  // All that survives a reload is the text: the earlier verdict is named in it.
  const a = request('a.txt grew without a test', [findings[0]], 100)
  const b = request('the panic is still there', [findings[2]], 200)
  const draft = openAiRequest('and rename the flag', a).text
  const again = openAiRequest(draft, b)
  expect(again.text).not.toContain('a.txt grew without a test')
  expect(again.text).not.toContain('cover it with a test')
  expect(again.text).toContain('the panic is still there')
  expect(again.text).toContain('return an error instead')
  expect(again.text).toContain('and rename the flag')
  expect(again.text).toContain(openTag(200))
  expect(again.text).not.toContain(openTag(100))
  expect(again.ticked).toEqual([true])
})

test('what is sent has no tags and stays marked as from the AI review', () => {
  const sent = markedFromAi(`${requestText(request('sum', [findings[0]]))}\n\nmine`)
  expect(sent).toBe(`${FROM_AI_REVIEW}\n\nsum\n\n- [should] a.txt:2 — cover it with a test\n\nmine`)
  expect(markedFromAi('x')).toBe(`${FROM_AI_REVIEW}\n\nx`)
})

test('a restored draft is known to hold a verdict by its tags', () => {
  expect(hasAiBlock(`mine\n\n${requestText(request('sum'))}`)).toBe(true)
  expect(hasAiBlock(`${FROM_AI_REVIEW}\n\nmine`)).toBe(false)
})

test('a reason sent from either composer never carries the tags', () => {
  // Request these changes…, then Back, then Reject: the shared text goes out
  // with the rejection, tags taken out and marked as the AI's.
  const opened = openAiRequest('', request('sum', [findings[0]], 7)).text
  for (const sent of [sentReason(opened), sentReason(`${opened}\n\nand mine`)]) {
    expect(sent).not.toContain('ai-review')
    expect(sent.startsWith(FROM_AI_REVIEW)).toBe(true)
  }
  // A tag line left behind on its own is taken out too.
  expect(sentReason(`${openTag(7)}\nmine`)).toBe('mine')
})

test('text written in place of the AI block is the operator’s own, not labelled as the AI’s', () => {
  const opened = openAiRequest('', request('sum', findings, 3)).text
  expect(sentReason(opened)).toContain(FROM_AI_REVIEW)
  // The operator clears the block and writes their own.
  expect(sentReason('please rename the flag')).toBe('please rename the flag')
})
