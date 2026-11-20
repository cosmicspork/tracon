<script lang="ts">
  // A record's address with a name (or an id prefix) where its id goes: ask
  // the node what it names, then swap the address for the record's own. A
  // name several records answer to lists them to pick from.
  import { api, type ResolvedRecord } from '../lib/api'
  import { formatAge } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import type { NameLookup } from '../lib/routes'

  let { lookup }: { lookup: NameLookup } = $props()

  let candidates = $state<ResolvedRecord[] | null>(null)
  let missing = $state(false)
  let failed = $state<string | null>(null)

  $effect(() => {
    const { kind, q, canonical } = lookup
    candidates = null
    missing = false
    failed = null
    let live = true
    api
      .resolve(kind, q)
      .then((r) => {
        if (!live) return
        if (r.found) router.replace(canonical(r.found.id))
        else if (r.candidates) candidates = r.candidates
        else missing = true
      })
      .catch((err) => {
        if (live) failed = err instanceof Error ? err.message : String(err)
      })
    return () => {
      live = false
    }
  })

  const noun = $derived(lookup.kind === 'task' ? 'task' : lookup.kind)
  const list = $derived(lookup.kind === 'task' ? '/tasks' : lookup.kind === 'session' ? '/sessions' : '/')
</script>

{#if candidates}
  <section class="lookup">
    <p><b>{lookup.q}</b> names {candidates.length} {noun}s. Pick one:</p>
    <ul>
      {#each candidates as c (c.kind + c.id)}
        <li>
          <a class="lnk" href={lookup.canonical(c.id)}>{c.name}</a>
          <small class="dim">{c.kind} · {c.channel}{c.created_ms ? ` · ${formatAge(c.created_ms)} ago` : ''} · <span class="mono" title={c.id}>{c.id}</span></small>
        </li>
      {/each}
    </ul>
  </section>
{:else if missing}
  <div class="banner crit">not found <b>· no {noun} is named {lookup.q}</b> <a class="lnk" href={list}>Back</a></div>
{:else if failed}
  <div class="banner crit">could not look up {lookup.q} <b>· {failed}</b></div>
{:else}
  <div class="empty">Looking up {lookup.q}…</div>
{/if}

<style>
  .lookup {
    padding: 18px;
  }
  ul {
    list-style: none;
    padding: 0;
    margin: 0.5em 0 0;
  }
  li {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5em;
    align-items: baseline;
    padding: 0.35em 0;
    border-bottom: 1px solid var(--rule);
    overflow-wrap: anywhere;
  }
  .mono {
    font-family: var(--mono);
  }
  .dim {
    color: var(--dim);
  }
</style>
