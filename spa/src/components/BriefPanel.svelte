<script lang="ts">
  // The item's product brief, where the item is read. Optional throughout:
  // an item without one shows a line saying so and an offer, never a gap
  // where a required field should be.
  //
  // Two things are shown that a prettier panel would hide. Every line keeps
  // the word that says who is behind it, and a section with nothing in it
  // says what it does not say — a brief that has never heard from the
  // customer should look like one.
  import { api } from '../lib/api'
  import { FIELDS, PROVENANCE, entriesOf, fieldHeading, nothingObserved, parseRefs, refHref, refLabel, restsOn } from '../lib/brief'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { surface } from '../lib/surface.svelte'
  import type { Brief, BriefField, Provenance, WorkView } from '../lib/types'

  let { item, brief, onchange }: { item: WorkView; brief: Brief | null; onchange: () => void } = $props()

  let busy = $state(false)
  let error = $state<string | null>(null)
  let adding = $state(false)
  let field = $state<BriefField>('problem')
  let provenance = $state<Provenance>('inferred')
  let text = $state('')
  let refsText = $state('')

  const parsed = $derived(parseRefs(refsText))

  async function act(f: () => Promise<unknown>) {
    busy = true
    error = null
    try {
      await f()
      onchange()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }

  function start() {
    return act(() => api.putBrief(item.id, { title: item.title }))
  }

  function add() {
    const line = text.trim()
    if (!line) return
    return act(async () => {
      await api.putBrief(item.id, {
        if_hash: brief?.hash,
        sections: [{ field, entries: [{ provenance, text: line, refs: parsed.refs }] }],
      })
      text = ''
      refsText = ''
      adding = false
    })
  }

  function unlink() {
    return act(() => api.unlinkBrief(item.id))
  }
</script>

<section class="brief">
  <div class="h5">
    Brief
    {#if brief}<b>{restsOn(brief)}</b>{/if}
  </div>

  {#if !brief && item.brief_slug}
    <div class="notice">
      This item points at <b>{item.brief_slug}</b>, which this node cannot read as a brief. The
      document may not have arrived from another node, or it may have been deleted or replaced.
    </div>
    {#if !surface.phone}
      <div class="actions"><button class="lnk d" onclick={unlink} disabled={busy}>Unlink</button></div>
    {/if}
  {:else if !brief}
    <div class="empty">
      No brief. This item is worked from its description and its plan, which is the ordinary case.
      A brief adds who the work is for, what would make it good, and what is still open.
    </div>
    {#if !surface.phone && item.state === 'open'}
      <div class="actions"><button class="btn" onclick={start} disabled={busy}>Start a brief</button></div>
    {/if}
  {:else}
    <div class="meta">
      <a href="/docs/{brief.channel}/{brief.slug}">{brief.slug}</a>
      · edited {formatAge(brief.updated_ms, clock.now)} ago
      {#if item.phase_plan_slug}· approach in <a href="/docs/{item.channel}/{item.phase_plan_slug}">{item.phase_plan_slug}</a>{/if}
    </div>
    {#if nothingObserved(brief)}
      <div class="notice">Nothing in this brief was observed: every line so far is someone's reasoning or decision.</div>
    {/if}
    {#if brief.preamble}<p class="prose">{brief.preamble}</p>{/if}

    {#each FIELDS as f (f)}
      {@const entries = entriesOf(brief, f)}
      {@const section = brief.sections.find((s) => s.field === f)}
      <div class="section">
        <div class="head">{fieldHeading(f)}</div>
        {#if section?.notes}<p class="prose">{section.notes}</p>{/if}
        {#if entries.length === 0}
          <div class="absent">{brief.absent.find((a) => a.field === f)?.says ?? 'nothing recorded'}</div>
        {:else}
          <ul>
            {#each entries as entry, i (i)}
              <li>
                <span class="mark {entry.provenance}" title={PROVENANCE[entry.provenance].title}>{PROVENANCE[entry.provenance].label}</span>
                <span class="text">{entry.text}</span>
                {#each entry.refs as r, j (j)}
                  {@const href = refHref(brief.channel, r)}
                  {#if href}
                    <a class="ref" class:unknown={r.known === false} {href}>{r.kind}: {refLabel(r)}</a>
                  {:else}
                    <span class="ref">{r.kind}: {refLabel(r)}</span>
                  {/if}
                {/each}
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    {/each}

    {#if brief.extra}
      <div class="section">
        <div class="head">Also in the document</div>
        <pre class="prose">{brief.extra}</pre>
      </div>
    {/if}

    {#if !surface.phone}
      {#if adding}
        <div class="add">
          <div class="row">
            <select bind:value={field} aria-label="section">
              {#each FIELDS as f (f)}<option value={f}>{fieldHeading(f)}</option>{/each}
            </select>
            <select bind:value={provenance} aria-label="who is behind this line">
              <option value="observed">observed · the customer said or did this</option>
              <option value="inferred">inferred · reasoned from something else</option>
              <option value="decided">decided · your decision</option>
              <option value="unattributed">unattributed</option>
            </select>
          </div>
          <input placeholder="the line, in one sentence" bind:value={text} onkeydown={(e) => e.key === 'Enter' && add()} />
          <input placeholder="what it rests on: doc:meeting-ops https://… work:… evidence:…" bind:value={refsText} />
          {#if parsed.rejected.length}
            <div class="notice">Not a reference: {parsed.rejected.join(', ')} · use doc, session, evidence, work, url or file</div>
          {/if}
          {#if provenance === 'observed' && parsed.refs.length === 0}
            <div class="notice">An observation with nothing to point at cannot be checked. Cite what was read, or record it as inferred.</div>
          {/if}
          <div class="actions">
            <button class="btn p" onclick={add} disabled={busy || !text.trim()}>Add the line</button>
            <button class="lnk" onclick={() => (adding = false)} disabled={busy}>Cancel</button>
          </div>
        </div>
      {:else}
        <div class="actions">
          <button class="btn" onclick={() => (adding = true)} disabled={busy}>Add a line</button>
          <a class="btn" href="/docs/{brief.channel}/{brief.slug}">Edit the document</a>
          <button class="lnk d" onclick={unlink} disabled={busy}>Unlink</button>
        </div>
      {/if}
    {/if}
  {/if}

  {#if error}<div class="banner crit">refused <b>· {error}</b></div>{/if}
</section>

<style>
  .brief {
    display: grid;
    gap: 8px;
    margin-top: 14px;
  }
  .h5 {
    font: 11.5px var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .h5 b {
    color: var(--ink2);
    font-weight: 400;
    margin-left: 8px;
    text-transform: none;
    letter-spacing: 0;
  }
  .meta {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .empty {
    max-width: 70ch;
    color: var(--ink2);
    font-size: 13.5px;
  }
  .notice {
    color: var(--wait);
    font: 12px var(--mono);
    max-width: 70ch;
  }
  .section {
    background: var(--s1);
    border-radius: 4px;
    padding: 8px 12px;
    max-width: 80ch;
  }
  .section .head {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
    margin-bottom: 4px;
  }
  .absent {
    color: var(--dim);
    font: 12.5px var(--mono);
  }
  .prose {
    margin: 4px 0;
    font-size: 13.5px;
    white-space: pre-wrap;
  }
  ul {
    margin: 0;
    padding: 0;
    list-style: none;
    display: grid;
    gap: 5px;
  }
  li {
    font-size: 13.5px;
    line-height: 1.45;
  }
  .mark {
    font: 11px var(--mono);
    letter-spacing: 0.04em;
    padding: 1px 5px;
    border-radius: 3px;
    background: var(--wash-dim);
    color: var(--dim);
    margin-right: 6px;
  }
  .mark.observed {
    background: var(--wash-ok);
    color: var(--ok);
  }
  .mark.inferred {
    background: var(--wash-wait);
    color: var(--wait);
  }
  .mark.decided {
    background: var(--wash-run);
    color: var(--acc);
  }
  .ref {
    font: 11.5px var(--mono);
    color: var(--ink2);
    margin-left: 7px;
    white-space: nowrap;
  }
  .ref.unknown {
    color: var(--dim);
    text-decoration: line-through dotted;
  }
  .add {
    display: grid;
    gap: 7px;
    max-width: 640px;
  }
  .add .row {
    display: flex;
    gap: 8px;
  }
  .add input,
  .add select {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--sans);
  }
  .add .row select {
    flex: 1;
    min-width: 0;
  }
  .actions {
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }
  .actions a.btn {
    text-decoration: none;
  }
</style>
