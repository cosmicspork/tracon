<script lang="ts">
  // What the operator opens onto: a place to start work, then what is waiting
  // on them, then what is running, then what landed. The queue that used to be
  // here showed two empty boxes above a wall of ended sessions — true, and no
  // use. Starting something is the first thing on the page instead.
  import Composer from '../components/Composer.svelte'
  import PermissionCard from '../components/PermissionCard.svelte'
  import PromotionCard from '../components/PromotionCard.svelte'
  import ReviewCard from '../components/ReviewCard.svelte'
  import SessionRow from '../components/SessionRow.svelte'
  import SetupCard from '../components/SetupCard.svelte'
  import { api } from '../lib/api'
  import { setupSteps } from '../lib/firstrun'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { Session, WorkView } from '../lib/types'

  const waiting = $derived(store.queue.waiting)
  const reviews = $derived(store.queue.reviews)
  const promotions = $derived(store.queue.promotions ?? [])
  const running = $derived(store.queue.running)
  // The home shows what landed lately; the whole history has its own screen.
  const landed = $derived(store.queue.ended.slice(0, 6))
  const ready = $derived(
    setupSteps({
      anyProviderConnected: store.providers.some((p) => p.state === 'connected'),
      anyChannel: store.channels.some((c) => !c.archived),
      hubPaired: store.mesh?.hub.state === 'connected',
    }) === null,
  )

  // Putting a session away leaves it whole; the stream carries the change
  // back, so nothing here has to guess what the list looks like afterwards.
  let busy = $state(false)
  function archive(s: Session) {
    void api.archiveSession(s.id).catch(() => store.refetch())
  }
  async function archiveAll() {
    busy = true
    try {
      await api.archiveEnded()
    } catch {
      await store.refetch()
    } finally {
      busy = false
    }
  }

  // Addressed with an item: the work screen sending a phase here to start.
  const params = $derived(new URLSearchParams(router.search))
  const itemId = $derived(params.get('item'))
  const phase = $derived<'plan' | 'execute'>(params.get('phase') === 'execute' ? 'execute' : 'plan')
  let item = $state<WorkView | null>(null)
  $effect(() => {
    const id = itemId
    if (!id) {
      item = null
      return
    }
    api
      .workItem(id)
      .then((d) => (item = d.item))
      .catch(() => (item = null))
  })
</script>

{#if ready}
  {#key itemId}
    <Composer {item} {phase} />
  {/key}
{:else}
  <SetupCard />
{/if}

{#if waiting.length || reviews.length || promotions.length}
  <div class="h4">
    Waiting on you <b>{waiting.length + reviews.length + promotions.length} · requests before reviews · oldest first</b>
  </div>
  <div class="rows">
    <!-- Requests expire; reviews do not. Requests come first. -->
    {#each waiting as p (p.id)}
      <PermissionCard permission={p} />
    {/each}
    {#each reviews as r (r.id)}
      <ReviewCard review={r} />
    {/each}
    <!-- Batches last: they neither expire nor hold an agent. -->
    {#each promotions as p (p.id)}
      <PromotionCard promotion={p} />
    {/each}
  </div>
{/if}

{#if running.length}
  <div class="h4">Running <b>{running.length}</b></div>
  <div class="rows">
    {#each running as s (s.id)}
      <SessionRow session={s} />
    {/each}
  </div>
{/if}

{#if landed.length}
  <div class="h4">
    Landed recently <b>{landed.length}</b>
    <span class="r">
      <button class="lnk" onclick={archiveAll} disabled={busy}>{busy ? 'Archiving…' : 'archive all'}</button>
      <a href="/sessions">all sessions</a>
    </span>
  </div>
  <div class="rows">
    {#each landed as s (s.id)}
      <SessionRow session={s} onarchive={archive} />
    {/each}
  </div>
{/if}

<style>
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .h4 .r {
    margin-left: auto;
    display: flex;
    gap: 14px;
    align-items: baseline;
    font: 12.5px var(--sans);
    letter-spacing: 0;
    text-transform: none;
  }
  .h4 .r .lnk {
    font-size: 12.5px;
  }
</style>
