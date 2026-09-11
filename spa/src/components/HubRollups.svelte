<script lang="ts">
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import type { HubRollups } from '../lib/types'

  let summaries = $state<Record<string, HubRollups>>({})
  let unavailable = $state<string | null>(null)
  let refreshTick = $state(0)
  let loadedKey = ''
  let loadedTick = -1
  let loading = false

  const channels = $derived(store.channels.filter((channel) => !channel.archived))
  const enabled = $derived(store.mesh?.hub.state === 'connected')

  onMount(() => {
    const timer = window.setInterval(() => (refreshTick += 1), 60_000)
    return () => window.clearInterval(timer)
  })

  async function load(key: string) {
    if (loading) return
    loading = true
    unavailable = null
    const fetched: Record<string, HubRollups> = {}
    let refusal: string | null = null
    await Promise.all(
      channels.map(async (channel) => {
        try {
          fetched[channel.name] = await api.hubRollups(channel.name)
        } catch (error) {
          // An unshared channel is not a failure of the node. Keep it out of
          // this optional panel; transport failure is visible below instead.
          const message = error instanceof Error ? error.message : String(error)
          if (!/shared a key|not explicitly shared|does not hold/i.test(message)) refusal ??= message
        }
      }),
    )
    if (loadedKey === key) summaries = fetched
    if (loadedKey === key) unavailable = refusal
    loading = false
    // A channel set can change while a fetch is running. Fetch its new key
    // rather than silently leaving the old set visible until another event.
    if (loadedKey && loadedKey !== key) void load(loadedKey)
  }

  $effect(() => {
    const key = enabled ? channels.map((channel) => channel.name).join('\u0000') : ''
    const tick = refreshTick
    if (!key) {
      loadedKey = ''
      loadedTick = tick
      summaries = {}
      unavailable = null
      return
    }
    if (key === loadedKey && tick === loadedTick) return
    loadedKey = key
    loadedTick = tick
    void load(key)
  })
</script>

{#if enabled && (Object.keys(summaries).length || unavailable)}
  <section class="rollups">
    <header>
      <span>Hub summaries</span>
      <small>optional · aggregates only</small>
    </header>
    {#if unavailable}
      <p class="off">Hub summary unavailable · {unavailable}. Local metrics and work remain on this node.</p>
    {/if}
    {#each Object.values(summaries) as rollup (rollup.channel)}
      <article class:complete={rollup.state === 'current_complete'} class:warn={rollup.state !== 'current_complete'}>
        <div class="head">
          <b>{rollup.channel}</b>
          <span class="chip" class:off={rollup.state === 'stale'}>{rollup.state.replace('_', ' ')}</span>
          <small>
            {rollup.coverage.current_complete
              ? 'all members current'
              : `${rollup.coverage.missing_nodes.length} missing · ${rollup.coverage.stale_nodes.length} stale · ${rollup.coverage.partial_nodes.length} partial`}
          </small>
        </div>
        {#each rollup.summaries as summary (summary.node_id)}
          <p>
            <code>{summary.node_id.slice(0, 8)}</code>
            · {summary.freshness}{summary.complete ? '' : ' · partial'}
            · {summary.session_counts.running ?? 0} running
            · {summary.queued_permissions} waiting
            · {summary.work_items} work
            · {summary.documents} docs
            · captured {formatAge(summary.captured_ms, clock.now)}
          </p>
        {/each}
      </article>
    {/each}
  </section>
{/if}

<style>
  .rollups { margin-top: 16px; display: grid; gap: 7px; }
  header { display: flex; gap: 9px; align-items: baseline; font: 600 13px var(--sans); }
  header small, p { font: 12px var(--mono); color: var(--ink2); }
  article { border-left: 3px solid var(--wait); background: var(--s1); padding: 8px 10px; }
  article.complete { border-left-color: var(--ok); }
  .head { display: flex; gap: 8px; align-items: baseline; }
  .head small { margin-left: auto; color: var(--dim); }
  p { margin: 5px 0 0; }
  .off { color: var(--dim); }
</style>
