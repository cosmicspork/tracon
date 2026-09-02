<script lang="ts">
  import Approval from './routes/Approval.svelte'
  import Doc from './routes/Doc.svelte'
  import Docs from './routes/Docs.svelte'
  import Metrics from './routes/Metrics.svelte'
  import Work from './routes/Work.svelte'
  import WorkItem from './routes/WorkItem.svelte'
  import Promotion from './routes/Promotion.svelte'
  import Home from './routes/Home.svelte'
  import Nodes from './routes/Nodes.svelte'
  import Sessions from './routes/Sessions.svelte'
  import Settings from './routes/Settings.svelte'
  import Session from './routes/Session.svelte'
  import Enroll from './routes/Enroll.svelte'
  import Login from './routes/Login.svelte'
  import { stashToken, tokenFromHash } from './lib/auth'
  import { clock } from './lib/clock.svelte'
  import { formatAge } from './lib/format'
  import { remedy } from './lib/refusal'
  import { router } from './lib/router.svelte'
  import { store } from './lib/store.svelte'
  import { surface } from './lib/surface.svelte'

  // A login QR lands with the token in the fragment. Stash it and strip the
  // address bar before anything else renders; if this browser is already
  // logged in the stash is simply never spent.
  {
    const t = tokenFromHash(location.hash)
    if (t) {
      stashToken(t)
      history.replaceState(null, '', location.pathname + location.search)
    }
  }

  router.start()
  surface.start()
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

  const waiting = $derived(
    store.queue.waiting.length + store.queue.reviews.length + (store.queue.promotions?.length ?? 0),
  )
  const sessionId = $derived(router.path.match(/^\/sessions\/([^/]+)/)?.[1] ?? null)
  const reviewId = $derived(router.path.match(/^\/reviews\/([^/]+)/)?.[1] ?? null)
  const promotionId = $derived(router.path.match(/^\/promotions\/([^/]+)/)?.[1] ?? null)
  const enroll = $derived(router.path === '/nodes/enroll')
  const settings = $derived(router.path === '/settings')
  const docRef = $derived(router.path.match(/^\/docs\/([^/]+)\/([^/]+)(\/edit)?$/))
  const docEdit = $derived(Boolean(docRef?.[3]))
  const workId = $derived(router.path.match(/^\/work\/([^/]+)/)?.[1] ?? null)
  const nav = $derived(
    settings
      ? 'settings'
      : router.path === '/nodes' || enroll || router.path === '/metrics'
        ? 'nodes'
        : router.path.startsWith('/docs')
          ? 'docs'
          : router.path.startsWith('/work')
            ? 'work'
            : router.path === '/sessions'
              ? 'sessions'
              : 'home',
  )
  const hubDown = $derived(store.mesh?.hub.state === 'unreachable')
  /** No hub is a thing to do something about, so it links to doing it. */
  const noHub = $derived(!store.mesh?.hub || store.mesh.hub.state === 'disabled')
  const hubLabel = $derived.by(() => {
    const hub = store.mesh?.hub
    if (!hub || hub.state === 'disabled') return 'no hub'
    if (hub.state === 'connected') return 'hub ok'
    return `hub down ${formatAge(hub.since_ms, clock.now)}`
  })
</script>

{#if store.authRequired}
  <Login />
{:else}
<div class="shell" class:narrow={collapsed}>
  <nav class="rail">
    {#if collapsed}
      <!-- Collapsed, the mark is the only thing at the top of the rail and it
           reads as a button. Make it one: the way back out. -->
      <button class="brand" type="button" onclick={toggleRail} aria-label="Expand navigation" title="Expand">
        <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9" /><path d="M12 12l6-6" /><circle cx="12" cy="12" r="1.4" fill="currentColor" /></svg>
      </button>
    {:else}
      <div class="brand">
        <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9" /><path d="M12 12l6-6" /><circle cx="12" cy="12" r="1.4" fill="currentColor" /></svg>
        <span class="lbl">tracon</span>
      </div>
    {/if}
    <a href="/" class:on={nav === 'home'}>
      <svg viewBox="0 0 24 24"><path d="M4 6h16M4 12h16M4 18h10" /></svg>
      <span class="lbl">Home</span>
      {#if waiting > 0}<span class="n">{waiting}</span>{/if}
    </a>
    <a href="/work" class:on={nav === 'work'}>
      <svg viewBox="0 0 24 24"><path d="M5 7h3M5 12h3M5 17h3" /><path d="M11 7h8M11 12h8M11 17h5" /></svg>
      <span class="lbl">Work</span>
    </a>
    <a href="/docs" class:on={nav === 'docs'}>
      <svg viewBox="0 0 24 24"><path d="M6 3h9l4 4v14H6z" /><path d="M9 11h7M9 15h7M9 7h3" /></svg>
      <span class="lbl">Documents</span>
    </a>
    <a href="/nodes" class:on={nav === 'nodes'}>
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="2.5" /><circle cx="5" cy="6" r="2" /><circle cx="19" cy="6" r="2" /><circle cx="12" cy="19" r="2" /><path d="M6.5 7.5l4 3M17.5 7.5l-4 3M12 14.5v2.5" /></svg>
      <span class="lbl">Nodes</span>
    </a>
    <span class="sp"></span>
    <a href="/settings" class:on={nav === 'settings'}>
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3" /><path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1" /></svg>
      <span class="lbl">Settings</span>
    </a>
    <div class="foot lbl" title="{store.node?.name ?? 'this node'} · {store.connected ? 'connected' : 'offline'} · {hubLabel}">
      <span>{store.node?.name ?? '…'}</span>
      <span
        >{store.connected ? 'connected' : 'offline'} ·
        {#if noHub}<a href="/settings#mesh">pair a hub</a>{:else}{hubLabel}{/if}</span
      >
    </div>
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
      <div class="banner crit">
        refusing to run harnesses <b>· {store.node.failed_check}: {store.node.failed_detail}</b>
        <i>{remedy(store.node.failed_check)}</i>
      </div>
    {/if}
    <!-- Degraded is a state, not an error: quiet, persistent, not dismissable. -->
    {#if hubDown}
      <div class="banner dim">hub unreachable <b>· local sessions continue; approvals will be delivered when it returns; search is text-only</b></div>
    {:else if store.reconnected !== null}
      <div class="banner ok">hub reconnected <b>· {store.reconnected} item{store.reconnected === 1 ? '' : 's'} delivered</b></div>
    {/if}
    {#if reviewId}
      <Approval id={reviewId} />
    {:else if promotionId}
      <Promotion id={promotionId} />
    {:else if sessionId}
      <Session id={sessionId} />
    {:else if nav === 'sessions'}
      <Sessions />
    {:else if enroll}
      <Enroll />
    {:else if router.path === '/metrics'}
      <Metrics />
    {:else if nav === 'nodes'}
      <Nodes />
    {:else if workId}
      <WorkItem id={workId} />
    {:else if nav === 'work'}
      <Work />
    {:else if docRef}
      <Doc channel={docRef[1]} slug={docRef[2]} edit={docEdit} />
    {:else if nav === 'docs'}
      <Docs />
    {:else if settings}
      <Settings />
    {:else}
      <Home />
    {/if}
  </main>

  <!-- The phone navigates from the bottom, where a thumb is. Capability is
       gated by surface, not by width: this is the same five destinations. -->
  <nav class="tabs">
    <a href="/" class:on={nav === 'home'}>
      <svg viewBox="0 0 24 24"><path d="M4 6h16M4 12h16M4 18h10" /></svg>
      <span>Home</span>
      {#if waiting > 0}<i class="dot"></i>{/if}
    </a>
    <a href="/work" class:on={nav === 'work'}>
      <svg viewBox="0 0 24 24"><path d="M5 7h3M5 12h3M5 17h3" /><path d="M11 7h8M11 12h8M11 17h5" /></svg>
      <span>Work</span>
    </a>
    <a href="/docs" class:on={nav === 'docs'}>
      <svg viewBox="0 0 24 24"><path d="M6 3h9l4 4v14H6z" /><path d="M9 11h7M9 15h7M9 7h3" /></svg>
      <span>Docs</span>
    </a>
    <a href="/nodes" class:on={nav === 'nodes'}>
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="2.5" /><circle cx="5" cy="6" r="2" /><circle cx="19" cy="6" r="2" /><circle cx="12" cy="19" r="2" /><path d="M6.5 7.5l4 3M17.5 7.5l-4 3M12 14.5v2.5" /></svg>
      <span>Nodes</span>
    </a>
    <a href="/settings" class:on={nav === 'settings'}>
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3" /><path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1" /></svg>
      <span>Setup</span>
    </a>
  </nav>
</div>
{/if}

<style>
  .shell { display: grid; grid-template-columns: 172px minmax(0, 1fr); min-height: 100vh; }
  .shell.narrow { grid-template-columns: 52px minmax(0, 1fr); }
  .rail { background: var(--s1); display: flex; flex-direction: column; padding: 12px 0; gap: 2px; overflow: hidden; position: sticky; top: 0; height: 100vh; }
  button.brand { background: none; border: 0; cursor: pointer; color: inherit; width: 100%; }
  .brand { display: flex; align-items: center; gap: 10px; padding: 4px 16px 14px; font: 600 15px var(--sans); letter-spacing: 0.06em; text-transform: uppercase; white-space: nowrap; }
  .rail svg { width: 17px; height: 17px; flex-shrink: 0; stroke: currentColor; fill: none; stroke-width: 1.7; stroke-linecap: round; stroke-linejoin: round; }
  .rail a, .rail button { display: flex; align-items: center; gap: 10px; padding: 8px 16px; color: var(--ink2); text-decoration: none; font: 500 14px var(--sans); white-space: nowrap; position: relative; background: none; border: 0; cursor: pointer; text-align: left; }
  .rail a.on { color: var(--ink); box-shadow: inset 2px 0 0 var(--acc); background: var(--s2); }
  .lbl { flex: 1; }
  .n { font: 12px var(--mono); color: var(--wait); }
  .sp { flex: 1; }
  /* `lbl` is what hides this when the rail is narrow, and `lbl` also grows.
     A footer that grows strands its own text at the top of the leftover
     space, which is where it sat. It sits at the bottom because `sp` above
     it grows; this must not. */
  .foot { padding: 8px 16px; font: 11.5px var(--mono); color: var(--dim); display: flex; flex-direction: column; gap: 1px; min-width: 0; flex: none; }
  .foot span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .foot a { color: var(--acc); text-decoration: none; }
  .foot a:hover { text-decoration: underline; }
  .rail button { color: var(--dim); }
  .rail button:hover { color: var(--ink); }
  .narrow .lbl { display: none; }
  .narrow .rail a, .narrow .rail button, .narrow .brand { padding-left: 0; padding-right: 0; justify-content: center; }
  .narrow .n { position: absolute; top: 2px; right: 8px; font-size: 10px; }
  .narrow .collapse svg { transform: rotate(180deg); }
  main { padding: 18px 22px 22px; display: flex; flex-direction: column; gap: 16px; min-width: 0; }
  /* Bottom tabs are the phone's navigation; the rail is the desktop's. */
  .tabs { display: none; }

  @media (max-width: 700px) {
    .shell, .shell.narrow { grid-template-columns: minmax(0, 1fr); }
    .rail { display: none; }
    main { padding: 14px 12px calc(72px + env(safe-area-inset-bottom)); }
    .tabs {
      display: grid;
      grid-template-columns: repeat(5, 1fr);
      position: fixed;
      inset: auto 0 0 0;
      background: var(--s1);
      border-top: 1px solid var(--rule);
      padding-bottom: env(safe-area-inset-bottom);
      z-index: 10;
    }
    .tabs a {
      display: flex;
      flex-direction: column;
      align-items: center;
      gap: 3px;
      padding: 9px 0 11px;
      color: var(--dim);
      text-decoration: none;
      font: 500 11px var(--sans);
      position: relative;
    }
    .tabs a.on { color: var(--ink); box-shadow: inset 0 2px 0 var(--acc); }
    .tabs svg {
      width: 19px; height: 19px; stroke: currentColor; fill: none;
      stroke-width: 1.7; stroke-linecap: round; stroke-linejoin: round;
    }
    .tabs .dot {
      position: absolute; top: 7px; right: 50%; margin-right: -16px;
      width: 6px; height: 6px; border-radius: 50%; background: var(--wait);
    }
  }
</style>
