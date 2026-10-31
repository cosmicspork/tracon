import { expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'

// The review page's markup, read as source: where a section sits is the
// order its tags appear in.
const review = readFileSync(new URL('./Review.svelte', import.meta.url), 'utf8')
const markup = review.slice(review.indexOf('</script>'))

test('shown work, the submitter note among it, comes before the evidence grid and the files', () => {
  const shown = markup.indexOf('<ShownWork ')
  expect(shown).toBeGreaterThan(-1)
  expect(markup.indexOf('<ShownWork ', shown + 1)).toBe(-1)
  const evidence = markup.indexOf('<section class="evidence">')
  const files = markup.indexOf('Files <b>')
  expect(evidence).toBeGreaterThan(-1)
  expect(files).toBeGreaterThan(-1)
  expect(shown).toBeLessThan(evidence)
  expect(shown).toBeLessThan(files)
})

test("the reviewer's verdict and its controls come before shown work, so a phone reaches it first", () => {
  const verdict = markup.indexOf('<div class="verdict"')
  const control = markup.indexOf("{#if control?.kind === 'running'}")
  const shown = markup.indexOf('<ShownWork ')
  expect(verdict).toBeGreaterThan(-1)
  expect(control).toBeGreaterThan(-1)
  expect(verdict).toBeLessThan(shown)
  expect(control).toBeLessThan(shown)
  // It follows the header's publish line, which says what is being decided.
  expect(markup.indexOf('<dl class="kv">')).toBeLessThan(verdict)
  // The provenance line keeps who reviewed it.
  expect(markup.indexOf('<dl class="prov">')).toBeGreaterThan(shown)
})
