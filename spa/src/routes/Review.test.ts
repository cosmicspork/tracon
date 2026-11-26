import { expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'

// The review page's markup, read as source: where a section sits is the
// order its tags appear in.
const review = readFileSync(new URL('./Review.svelte', import.meta.url), 'utf8')
const markup = review.slice(review.indexOf('</script>'))

test('shown work, the submitter note among it, comes before the files, and the evidence grid and provenance are gone', () => {
  const shown = markup.indexOf('<ShownWork ')
  expect(shown).toBeGreaterThan(-1)
  expect(markup.indexOf('<ShownWork ', shown + 1)).toBe(-1)
  const files = markup.indexOf('Files <b>')
  expect(files).toBeGreaterThan(-1)
  expect(shown).toBeLessThan(files)
  // The status line says what they said, in one line under the title.
  expect(markup).not.toContain('<section class="evidence">')
  expect(markup).not.toContain('Candidate evidence')
  expect(markup).not.toContain('<dl class="prov">')
  expect(markup).not.toContain('<div class="recover">\n      {#if control')
  // A review from before capture still says so, as a banner.
  expect(markup).toContain('verification evidence missing')
})

test('the header names the review with its id chip, and drops the claimed jargon', () => {
  const head = markup.slice(markup.indexOf('<div\n    class="head"'), markup.indexOf('<div class="status"'))
  expect(head).toContain("<ReviewId kind={review.kind === 'report' ? 'report' : 'review'} id={review.id} />")
  expect(head).not.toContain('claimed')
})

test('one status line sits under the title, before the publish line and the verdict card', () => {
  const head = markup.indexOf('class="head"')
  const status = markup.indexOf('<div class="status"')
  const verdict = markup.indexOf('<div class="verdict"')
  const shown = markup.indexOf('<ShownWork ')
  expect(status).toBeGreaterThan(head)
  expect(status).toBeLessThan(markup.indexOf('<dl class="kv">'))
  expect(markup.indexOf('<dl class="kv">')).toBeLessThan(verdict)
  expect(verdict).toBeLessThan(shown)
  const line = markup.slice(status, markup.indexOf('</div>', markup.indexOf('{/if}\n  </div>', status)))
  // Checks, reviewer, task, docs, model and who submitted it, each on the line.
  for (const item of ['checkStatus.label', 'reviewerItem.kind', '{#if taskId}', 'docs {evidence.demonstrations.length}', '{session.model} · {session.phase}', 'submitted by', 'kind="session"']) {
    expect(line).toContain(item)
  }
  // The Publishes line no longer repeats the session.
  const kv = markup.slice(markup.indexOf('<dl class="kv">'), markup.indexOf('</dl>'))
  expect(kv).not.toContain('Submitted by')
})

test("the reviewer item offers each control the old row did, and the owner's refusal beside them", () => {
  const status = markup.slice(markup.indexOf('<div class="status"'))
  const line = status.slice(0, status.indexOf('{#if expanded ==='))
  for (const label of ['Start reviewer', 'Retry reviewer', 'Review this revision', 'Review again…', 'Settings → Channels', 'reviewing…']) {
    expect(line).toContain(label)
  }
  expect(line).toContain('{#if reviewerRefusal}')
  expect(review).toContain('away: reviewerAway')
  expect(review).toContain("ownerDetail?.state !== 'fetched' && review.review_session_id !== null")
})

test('each status item expands to the detail it replaced', () => {
  const at = markup.indexOf("{#if expanded === 'checks'}")
  expect(at).toBeGreaterThan(markup.indexOf('<div class="status"'))
  const detail = markup.slice(at, markup.indexOf('{/if}\n\n', at))
  expect(detail).toContain('{#each authoritativeChecks as run (run.id)}')
  expect(detail).toContain('onclick={() => (openRun = run)}')
  expect(detail).toContain('onclick={() => fold(run.id)}')
  expect(detail).toContain("{:else if expanded === 'task' && requirements}")
  expect(detail).toContain('renderMessage(requirements.body)')
  expect(detail).toContain("{:else if expanded === 'docs'")
  expect(detail).toContain('{#each evidence.demonstrations as demo (demo.id)}')
  expect(detail).toContain('{#each checks as c, index (index)}')
})

test('under generated details the opener sits above the agent text, which is collapsed and still editable', () => {
  const at = markup.indexOf('{#snippet generated(')
  expect(at).toBeGreaterThan(-1)
  const snippet = markup.slice(at, markup.indexOf('{/snippet}', at))
  const opener = snippet.indexOf('bind:value={opener}')
  const details = snippet.indexOf('<details class="generated">')
  expect(opener).toBeGreaterThan(-1)
  expect(snippet).toContain('placeholder="What this is and why it matters"')
  // Collapsed by default: no `open` on it.
  expect(details).toBeGreaterThan(opener)
  expect(snippet).not.toContain('<details class="generated" open')
  expect(snippet.slice(details)).toContain('{GENERATED_DETAILS}')
  expect(snippet.slice(details)).toContain('bind:value={body}')
  expect(snippet.slice(details)).toContain('bind:value={descBody}')
  // What the forge gets is previewed once, for every outgoing text, below.
  expect(snippet).not.toContain('<OutgoingPreview')
})

test('the outgoing title and body are previewed below their source, as approval sends them', () => {
  const at = markup.indexOf('<OutgoingPreview')
  expect(at).toBeGreaterThan(-1)
  expect(markup.indexOf('<OutgoingPreview', at + 1)).toBe(-1)
  // After every editor of the text it shows.
  expect(at).toBeGreaterThan(markup.indexOf('bind:value={title}'))
  expect(at).toBeGreaterThan(markup.indexOf('bind:value={descBody}'))
  expect(at).toBeGreaterThan(markup.indexOf('bind:value={comment}'))
  // The node is asked with what approve would send it.
  const preview = markup.slice(at, markup.indexOf('/>', at))
  expect(preview).toContain('outputs: remoteOwner ? undefined : outputs')
  expect(preview).toContain('opener: opening ? opener : undefined')
  expect(preview).toContain('revision_id: revision?.id')
  const decide = review.slice(review.indexOf('async function decide('))
  const call = decide.slice(0, decide.indexOf('})\n'))
  expect(call).toContain("outputs: verdict === 'approve' && !remoteOwner ? outputs : undefined")
  expect(call).toContain("opener: verdict === 'approve' && opening ? opener : undefined")
})

test('the opener shows only where approval sends a description, and only with the setting on', () => {
  expect(review).toContain('openerFor(generatedDetails && !closed, { change: change !== null, initial, describe })')
  // A new change's body, or the description replacing a change's; otherwise the plain editor.
  expect(markup).toContain("{#if opening === 'body'}\n      {@render generated('body')}\n    {:else}")
  expect(markup).toContain("{#if opening === 'description'}\n      {@render generated('description')}\n    {:else}")
  // Nothing else renders it: "Comment on the …" never gets one.
  expect(markup.split("{@render generated(").length - 1).toBe(2)
})

test('approval is gated on the opener and carries it to the node, which composes the body', () => {
  const conditions = review.slice(review.indexOf('const conditions = $derived('))
  expect(conditions.slice(0, conditions.indexOf('})\n'))).toContain('opener: opening')
  expect(review).toContain("where: remoteOwner ? nodeLabel(store.nodes, remoteOwner) : surface.phone ? 'the desktop' : null")
  const decide = review.slice(review.indexOf('async function decide('))
  const call = decide.slice(0, decide.indexOf('})\n'))
  expect(call).toContain("opener: verdict === 'approve' && opening ? opener : undefined")
  // The body and description go as written; the wrapping is the node's.
  expect(call).toContain("body: verdict === 'approve' ? body : undefined")
  expect(call).not.toContain('composed')
})

test('the opener is part of the review draft', () => {
  const fields = review.slice(review.indexOf('function draftFields()'))
  expect(fields.slice(0, fields.indexOf('\n  }\n'))).toContain('opener: opener || undefined')
  const apply = review.slice(review.indexOf('function applyDraft('))
  expect(apply.slice(0, apply.indexOf('\n  }\n'))).toContain('if (d.opener !== undefined) opener = d.opener')
})

test('review threads sit after the diff and before the verdict bar, and start from a line picked in the full diff only', () => {
  const diff = markup.indexOf('<ReviewDiff\n        bind:this={reviewDiff}\n        diff={review.diff}')
  const threads = markup.indexOf('<ReviewThreads')
  const bar = markup.indexOf('<div class="decide"')
  expect(diff).toBeGreaterThan(-1)
  expect(threads).toBeGreaterThan(diff)
  expect(threads).toBeLessThan(bar)
  // The full diff is this revision's, numbered as the node anchors; the
  // since-last-verdict diff is not, and offers no picks.
  expect(markup.slice(diff, threads)).toContain('onpick={threadsBlocked ? undefined : pickLine}')
  expect(markup.slice(diff, threads)).toContain('marks={threadMarks}')
  const since = markup.slice(markup.indexOf('diff={sinceReviewed.diff}'))
  expect(since.slice(0, since.indexOf('/>'))).not.toContain('onpick')
})

test('unresolved threads let a request for changes go without a note', () => {
  const conditions = review.slice(review.indexOf('const conditions = $derived('))
  expect(conditions.slice(0, conditions.indexOf('})\n'))).toContain('openThreads: unresolvedCount(threads)')
  // Read with the rest of the review, and again with it.
  expect(review.split('threads = d.threads ??').length - 1).toBe(2)
})
