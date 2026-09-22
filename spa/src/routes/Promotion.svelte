<script lang="ts">
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { Promotion, PromotionItem, PromotionVerdict } from '../lib/types'

  let { id }: { id: string } = $props()

  let promotion = $state<Promotion | null>(null)
  let items = $state<PromotionItem[]>([])
  let verdicts = $state<Record<string, { verdict: 'promote' | 'reject'; body?: string }>>({})
  let saved = $state<Record<string, PromotionVerdict>>({})
  let busy = $state(false)
  let error = $state<string | null>(null)
  let loaded = $state(false)

  function verdictOf(v: PromotionVerdict | undefined): 'promote' | 'reject' | undefined {
    return typeof v === 'string' ? v : v?.verdict
  }
  function bodyOf(v: PromotionVerdict | undefined): string | undefined {
    return typeof v === 'object' ? v.body : undefined
  }

  $effect(() => {
    void id
    loaded = false
    api
      .promotion(id)
      .then((d) => {
        promotion = d.promotion
        items = d.items
        saved = d.verdicts
        loaded = true
      })
      .catch((e) => {
        error = e instanceof Error ? e.message : String(e)
        loaded = true
      })
  })

  const pending = $derived(items.filter((i) => !saved[i.memory_id]))
  const chosen = $derived(Object.keys(verdicts).length)

  function pick(memoryId: string, v: 'promote' | 'reject', defaultBody: string) {
    const current = verdicts[memoryId]
    verdicts = { ...verdicts, [memoryId]: { verdict: v, body: current?.body ?? defaultBody } }
  }
  function editBody(memoryId: string, body: string) {
    const current = verdicts[memoryId]
    if (!current) return
    verdicts = { ...verdicts, [memoryId]: { ...current, body } }
  }
  function all(v: 'promote' | 'reject') {
    const next: typeof verdicts = {}
    for (const i of pending) next[i.memory_id] = { verdict: v, body: i.body }
    verdicts = next
  }

  async function send() {
    if (!chosen) return
    busy = true
    error = null
    try {
      const res = await api.decidePromotion(id, verdicts)
      await store.refetch()
      if (res.state === 'decided') {
        router.go('/')
      } else {
        const d = await api.promotion(id)
        saved = d.verdicts
        verdicts = {}
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

{#if !loaded}
  <div class="empty">Loading…</div>
{:else if !promotion}
  <div class="empty">{error ?? 'No such batch.'}</div>
{:else}
  <div class="h4">
    Memory batch
    <b>{promotion.channel} · {formatAge(promotion.created_ms, clock.now)} · {items.length} proposed{promotion.state === 'decided' ? ' · decided' : ''}</b>
  </div>
  <p class="lede">
    What agents retained and were told would wait. Promoted lessons are recalled by later sessions;
    rejected ones are never injected. Nothing here is context until you say so.
  </p>
  {#if promotion.state === 'open' && pending.length > 1}
    <div class="bulk">
      <button class="lnk" onclick={() => all('promote')}>Promote all</button>
      <button class="lnk d" onclick={() => all('reject')}>Reject all</button>
    </div>
  {/if}
  <div class="rows">
    {#each items as it (it.memory_id)}
      {@const done = saved[it.memory_id]}
      {@const doneVerdict = verdictOf(done)}
      {@const entry = verdicts[it.memory_id]}
      {@const v = entry?.verdict}
      <div class="item" class:promote={v === 'promote' || doneVerdict === 'promote'} class:reject={v === 'reject' || doneVerdict === 'reject'}>
        <span class="bar"></span>
        <span class="body">
          <em>{it.kind}</em>
          {#if !done && v === 'promote'}
            <textarea
              value={entry?.body ?? it.body}
              oninput={(e) => editBody(it.memory_id, (e.currentTarget as HTMLTextAreaElement).value)}
              spellcheck="false"
            ></textarea>
          {:else}
            {bodyOf(done) ?? it.body}
          {/if}
          <small
            >{it.scope}{it.scope_ref ? ` · ${it.scope_ref.slice(0, 8)}` : ''} · {Math.round(it.confidence * 100)}% ·
            {formatAge(it.created_ms, clock.now)}{it.source_session ? ` · session ${it.source_session.slice(-6)}` : ''}</small
          >
        </span>
        <span class="act">
          {#if done}
            <span class="chip" class:bad={doneVerdict === 'reject'}>{doneVerdict === 'promote' ? 'promoted' : 'rejected'}</span>
          {:else}
            <button class="lnk d" class:on={v === 'reject'} onclick={() => pick(it.memory_id, 'reject', it.body)}>Reject</button>
            <button class="btn" class:p={v === 'promote'} onclick={() => pick(it.memory_id, 'promote', it.body)}>Promote</button>
          {/if}
        </span>
      </div>
    {/each}
  </div>
  {#if promotion.state === 'open'}
    <div class="send">
      <button class="btn p" disabled={busy || !chosen} onclick={send}
        >Send {chosen ? `${chosen} verdict${chosen === 1 ? '' : 's'}` : 'verdicts'}</button
      >
      <a class="lnk" href="/">Back</a>
      {#if error}<span class="err">{error}</span>{/if}
    </div>
  {/if}
{/if}

<style>
  .lede {
    color: var(--ink2);
    max-width: 62ch;
    margin: 0 0 12px;
  }
  .bulk {
    display: flex;
    gap: 14px;
    margin-bottom: 8px;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .item {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    background: var(--s1);
    border-radius: 4px;
    padding: 11px 14px 11px 0;
  }
  .bar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--wait);
  }
  .item.promote .bar {
    background: var(--ok);
  }
  .item.reject .bar {
    background: var(--dim);
  }
  .item.reject .body {
    color: var(--dim);
  }
  .body {
    min-width: 0;
    white-space: pre-wrap;
  }
  .body em {
    font: 11.5px var(--mono);
    font-style: normal;
    color: var(--wait);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    margin-right: 8px;
  }
  .body small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    margin-top: 3px;
    white-space: normal;
  }
  .body textarea {
    display: block;
    width: 100%;
    min-height: 4.5em;
    font: inherit;
    color: inherit;
    background: var(--s2);
    border: 0;
    border-radius: 3px;
    padding: 6px 8px;
    resize: vertical;
    box-sizing: border-box;
  }
  .act {
    display: flex;
    gap: 10px;
    align-items: center;
    white-space: nowrap;
  }
  .lnk.on {
    text-decoration: underline;
  }
  .send {
    display: flex;
    gap: 14px;
    align-items: center;
    margin-top: 14px;
  }
  .err {
    color: var(--crit);
    font: 12.5px var(--mono);
  }
  @media (max-width: 700px) {
    .item {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 6px 12px;
    }
    .act {
      grid-column: 2;
    }
  }
</style>
