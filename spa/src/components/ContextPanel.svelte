<script lang="ts">
  // The documents the operator selected for this item, and what every attempt
  // at it actually received.
  //
  // Optional like the brief: an item with nothing selected is worked from its
  // description, its brief pointer and its plan. What is shown that a tidier
  // panel would hide: a selected document the next attempt will not get, and
  // an attempt whose context was different from the one before it.
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { ROLES, attemptLine, deliveryLine, picksInOrder, reasonLine, roleHeading, withPick, without } from '../lib/context'
  import { formatAge } from '../lib/format'
  import { surface } from '../lib/surface.svelte'
  import type { ContextPick, ContextRole, Document, WorkContext, WorkView } from '../lib/types'

  let { item }: { item: WorkView } = $props()

  let ctx = $state<WorkContext | null>(null)
  let docs = $state<Document[]>([])
  let busy = $state(false)
  let error = $state<string | null>(null)
  let adding = $state(false)
  let role = $state<ContextRole>('research')
  let slug = $state('')
  let note = $state('')

  async function load() {
    try {
      ctx = await api.workContext(item.id)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    }
  }
  $effect(() => {
    void item.id
    void item.updated_ms
    void load()
  })

  const selection = $derived(ctx?.selection ?? null)
  const picks = $derived(picksInOrder(selection))
  const selected = $derived(new Set(picks.map((p) => p.slug)))
  // Documents worth offering: markdown the node can carry, not another
  // item's selection, not archived, not already picked.
  const offer = $derived(
    docs.filter((d) => d.format === 'markdown' && d.kind !== 'context' && !d.archived && !selected.has(d.slug)),
  )

  async function save(next: ContextPick[]) {
    busy = true
    error = null
    try {
      ctx = await api.putContext(item.id, { picks: next, if_hash: selection?.hash })
      return true
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      await load()
      return false
    } finally {
      busy = false
    }
  }

  async function openAdd() {
    adding = true
    if (docs.length === 0) {
      try {
        docs = (await api.docs(item.channel)).docs
      } catch (e) {
        error = e instanceof Error ? e.message : String(e)
      }
    }
  }

  async function start() {
    // The brief the item already points at is the obvious first pick.
    const first: ContextPick[] = item.brief_slug ? [{ role: 'brief', slug: item.brief_slug, note: '' }] : []
    if (await save(first)) await openAdd()
  }

  async function add() {
    if (!slug.trim()) return
    if (await save(withPick(picks, { role, slug, note }))) {
      slug = ''
      note = ''
      adding = false
    }
  }

  function remove(s: string) {
    return save(without(picks, s))
  }
</script>

<section class="context">
  <div class="h5">
    Context
    {#if selection}<b>{picks.length} {picks.length === 1 ? 'document' : 'documents'} selected</b>{/if}
  </div>

  {#if !selection}
    <div class="empty">
      Nothing selected. Attempts at this item start from its description, its brief pointer and its plan. Select the
      brief, research, decisions and constraints that every attempt should start with, in full.
    </div>
    {#if !surface.phone && item.state === 'open'}
      <div class="actions"><button class="btn" onclick={start} disabled={busy}>Select context</button></div>
    {/if}
  {:else}
    <div class="meta">
      <a href="/docs/{selection.channel}/{selection.slug}">{selection.slug}</a>
      · edited {formatAge(selection.updated_ms, clock.now)} ago · changes apply to the next attempt
    </div>

    {#each ROLES as r (r)}
      {@const inRole = picks.filter((p) => p.role === r)}
      {#if inRole.length}
        <div class="section">
          <div class="head">{roleHeading(r)}</div>
          <ul>
            {#each inRole as p (p.slug)}
              {@const res = selection.resolved.find((x) => x.slug === p.slug)}
              <li>
                <a href="/docs/{selection.channel}/{p.slug}" class:unknown={res && !res.known}>{res?.title ?? p.slug}</a>
                {#if res?.title}<span class="slug">{p.slug}</span>{/if}
                {#if p.note}<span class="note">{p.note}</span>{/if}
                {#if !surface.phone}<button class="lnk d" onclick={() => remove(p.slug)} disabled={busy} aria-label="remove {p.slug}">×</button>{/if}
                {#if res?.reason}<span class="warn">next attempt will not get it · {reasonLine(res.reason)}</span>{/if}
              </li>
            {/each}
          </ul>
        </div>
      {/if}
    {/each}
    {#if picks.length === 0}
      <div class="absent">No documents in the selection. The next attempt is told its context is empty.</div>
    {/if}
    {#if selection.extra}
      <div class="section">
        <div class="head">Also in the document</div>
        <pre class="prose">{selection.extra}</pre>
      </div>
    {/if}

    {#if !surface.phone}
      {#if adding}
        <div class="add">
          <div class="row">
            <select bind:value={role} aria-label="role">
              {#each ROLES as r (r)}<option value={r}>{roleHeading(r)}</option>{/each}
            </select>
            <input list="context-docs-{item.id}" placeholder="document slug" bind:value={slug} />
            <datalist id="context-docs-{item.id}">
              {#each offer as d (d.id)}<option value={d.slug}>{d.title}</option>{/each}
            </datalist>
          </div>
          <input placeholder="why it belongs (optional)" bind:value={note} onkeydown={(e) => e.key === 'Enter' && add()} />
          <div class="actions">
            <button class="btn p" onclick={add} disabled={busy || !slug.trim()}>Add to context</button>
            <button class="lnk" onclick={() => (adding = false)} disabled={busy}>Cancel</button>
          </div>
        </div>
      {:else}
        <div class="actions">
          <button class="btn" onclick={openAdd} disabled={busy}>Add a document</button>
          <a class="btn" href="/docs/{selection.channel}/{selection.slug}">Edit the document</a>
        </div>
      {/if}
    {/if}
  {/if}

  {#if ctx && ctx.attempts.length}
    <div class="h5 later">Received by each attempt <b>{ctx.attempts.length}</b></div>
    <ul class="attempts">
      {#each ctx.attempts as a (a.session_id)}
        <li>
          <details>
            <summary class:moved={a.changes.length > 0} class:short={a.received.some((r) => r.delivery !== 'full')}>
              <a href="/sessions/{a.session_id}">{a.session_id.slice(-6)}</a>
              · {attemptLine(a)} · {formatAge(a.created_ms, clock.now)} ago
            </summary>
            {#if a.changes.length}
              <ul class="changes">
                {#each a.changes as c, i (i)}<li class={c.kind}>{c.says}</li>{/each}
              </ul>
            {/if}
            <ul class="received">
              {#each a.received as r (r.slug)}
                <li class={r.delivery}>
                  <span class="role">{roleHeading(r.role)}</span>
                  {r.title ?? r.slug}{#if r.title}<span class="slug">{r.slug}</span>{/if} · {deliveryLine(r)}
                </li>
              {/each}
            </ul>
          </details>
        </li>
      {/each}
    </ul>
  {/if}

  {#if error}<div class="banner crit">refused <b>· {error}</b></div>{/if}
</section>

<style>
  .context {
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
  .meta,
  .absent {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .empty {
    max-width: 70ch;
    color: var(--ink2);
    font-size: 13.5px;
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
    overflow-wrap: anywhere;
  }
  .slug,
  .role {
    font: 11.5px var(--mono);
    color: var(--dim);
    margin-left: 4px;
  }
  .role {
    margin: 0 6px 0 0;
  }
  .note {
    color: var(--ink2);
    margin-left: 6px;
  }
  .note::before {
    content: '— ';
  }
  .warn {
    display: block;
    color: var(--wait);
    font: 12px var(--mono);
  }
  a.unknown {
    color: var(--wait);
  }
  .lnk.d {
    margin-left: 6px;
  }
  .prose {
    margin: 4px 0;
    font-size: 13.5px;
    white-space: pre-wrap;
  }
  .add {
    display: grid;
    gap: 6px;
    max-width: 80ch;
  }
  .add .row {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .add input,
  .add select {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--sans);
    min-width: 0;
  }
  .add .row input {
    flex: 1;
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
  .attempts summary {
    cursor: pointer;
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .attempts summary a {
    white-space: nowrap;
  }
  .h5.later {
    margin-top: 6px;
  }
  .attempts summary.moved,
  .attempts summary.short {
    color: var(--wait);
  }
  .changes,
  .received {
    margin: 6px 0 4px 14px;
    font-size: 13px;
  }
  .changes li {
    color: var(--ink2);
  }
  .received li.partial,
  .received li.omitted {
    color: var(--wait);
  }
</style>
