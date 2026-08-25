<script lang="ts">
  import Approval from './routes/Approval.svelte'
  import NewSession from './routes/NewSession.svelte'
  import Nodes from './routes/Nodes.svelte'
  import Queue from './routes/Queue.svelte'
  import Session from './routes/Session.svelte'
  import { router } from './lib/router.svelte'
  import { store } from './lib/store.svelte'

  router.start()
  store.connect()

  let collapsed = $state(false)
  try {
    collapsed = localStorage.getItem('tracon-rail') === 'narrow'
  } catch {
    /* blocked storage */
  }
  function toggleRail() {
    collapsed = !collapsed
    try {
      localStorage.setItem('tracon-rail', collapsed ? 'narrow' : 'wide')
    } catch {
      /* blocked storage */
    }
  }

  const waiting = $derived(store.queue.waiting.length + store.queue.reviews.length)
  const sessionId = $derived(router.path.match(/^\/sessions\/([^/]+)/)?.[1] ?? null)
  const reviewId = $derived(router.path.match(/^\/reviews\/([^/]+)/)?.[1] ?? null)
  const nav = $derived(
    sessionId || router.path === '/sessions'
      ? 'sessions'
      : router.path === '/nodes'
        ? 'nodes'
        : router.path === '/new'
          ? 'new'
          : 'queue',
  )
</script>

<div class="shell" class:narrow={collapsed}>
  <nav class="rail">
    <div class="brand">
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9" /><path d="M12 12l6-6" /><circle cx="12" cy="12" r="1.4" fill="currentColor" /></svg>
      <span class="lbl">tracon</span>
    </div>
    <a href="/" class:on={nav === 'queue'}>
      <svg viewBox="0 0 24 24"><path d="M4 6h16M4 12h16M4 18h10" /></svg>
      <span class="lbl">Queue</span>
      {#if waiting > 0}<span class="n">{waiting}</span>{/if}
    </a>
    <a href="/sessions" class:on={nav === 'sessions'}>
      <svg viewBox="0 0 24 24"><path d="M4 5h16v14H4z" /><path d="M8 10l3 2-3 2M13 14h4" /></svg>
      <span class="lbl">Sessions</span>
    </a>
    <a href="/nodes" class:on={nav === 'nodes'}>
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="2.5" /><circle cx="5" cy="6" r="2" /><circle cx="19" cy="6" r="2" /><circle cx="12" cy="19" r="2" /><path d="M6.5 7.5l4 3M17.5 7.5l-4 3M12 14.5v2.5" /></svg>
      <span class="lbl">Nodes</span>
    </a>
    <a href="/new" class:on={nav === 'new'}>
      <svg viewBox="0 0 24 24"><path d="M12 5v14M5 12h14" /></svg>
      <span class="lbl">New session</span>
    </a>
    <span class="sp"></span>
    <div class="foot lbl">{store.node?.name ?? '…'} · {store.connected ? 'connected' : 'offline'}</div>
    <button class="collapse" type="button" onclick={toggleRail} title="Collapse">
      <svg viewBox="0 0 24 24"><path d="M15 6l-6 6 6 6" /></svg>
      <span class="lbl">Collapse</span>
    </button>
  </nav>

  <main>
    {#if !store.connected}
      <div class="banner crit">node unreachable <b>· reconnecting; state lives on the node, nothing typed is lost</b></div>
    {/if}
    {#if store.node?.state === 'refused'}
      <div class="banner crit">refusing to run harnesses <b>· {store.node.failed_check}: {store.node.failed_detail}</b></div>
    {/if}
    {#if reviewId}
      <Approval id={reviewId} />
    {:else if sessionId}
      <Session id={sessionId} />
    {:else if nav === 'sessions'}
      <Queue only="sessions" />
    {:else if nav === 'nodes'}
      <Nodes />
    {:else if nav === 'new'}
      <NewSession />
    {:else}
      <Queue />
    {/if}
  </main>
</div>

<style>
  .shell { display: grid; grid-template-columns: 172px minmax(0, 1fr); min-height: 100vh; }
  .shell.narrow { grid-template-columns: 52px minmax(0, 1fr); }
  .rail { background: var(--s1); display: flex; flex-direction: column; padding: 12px 0; gap: 2px; overflow: hidden; position: sticky; top: 0; height: 100vh; }
  .brand { display: flex; align-items: center; gap: 10px; padding: 4px 16px 14px; font: 600 15px var(--sans); letter-spacing: 0.06em; text-transform: uppercase; white-space: nowrap; }
  .rail svg { width: 17px; height: 17px; flex-shrink: 0; stroke: currentColor; fill: none; stroke-width: 1.7; stroke-linecap: round; stroke-linejoin: round; }
  .rail a, .rail button { display: flex; align-items: center; gap: 10px; padding: 8px 16px; color: var(--ink2); text-decoration: none; font: 500 14px var(--sans); white-space: nowrap; position: relative; background: none; border: 0; cursor: pointer; text-align: left; }
  .rail a.on { color: var(--ink); box-shadow: inset 2px 0 0 var(--acc); background: var(--s2); }
  .lbl { flex: 1; }
  .n { font: 12px var(--mono); color: var(--wait); }
  .sp { flex: 1; }
  .foot { padding: 8px 16px; font: 11.5px var(--mono); color: var(--dim); white-space: nowrap; }
  .rail button { color: var(--dim); }
  .rail button:hover { color: var(--ink); }
  .narrow .lbl { display: none; }
  .narrow .rail a, .narrow .rail button, .narrow .brand { padding-left: 0; padding-right: 0; justify-content: center; }
  .narrow .n { position: absolute; top: 2px; right: 8px; font-size: 10px; }
  .narrow .collapse svg { transform: rotate(180deg); }
  main { padding: 18px 22px 22px; display: flex; flex-direction: column; gap: 16px; min-width: 0; }
  @media (max-width: 700px) {
    .shell, .shell.narrow { grid-template-columns: 52px minmax(0, 1fr); }
    .shell:not(.narrow) .lbl { display: none; }
  }
</style>
