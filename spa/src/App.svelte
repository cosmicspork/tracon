<script lang="ts">
  import Review from './routes/Review.svelte'
  import Approval from './routes/Approval.svelte'
  import Doc from './routes/Doc.svelte'
  import DocPreview from './routes/DocPreview.svelte'
  import Docs from './routes/Docs.svelte'
  import Memories from './routes/Memories.svelte'
  import Metrics from './routes/Metrics.svelte'
  import Work from './routes/Work.svelte'
  import WorkItem from './routes/WorkItem.svelte'
  import Promotion from './routes/Promotion.svelte'
  import Home from './routes/Home.svelte'
  import More from './routes/More.svelte'
  import Sessions from './routes/Sessions.svelte'
  import Settings from './routes/Settings.svelte'
  import Session from './routes/Session.svelte'
  import Enroll from './routes/Enroll.svelte'
  import Login from './routes/Login.svelte'
  import DeviceApproval from './routes/DeviceApproval.svelte'
  import PasskeyOffer from './components/PasskeyOffer.svelte'
  import StepUpPrompt from './components/StepUpPrompt.svelte'
  import RestartBanner from './components/RestartBanner.svelte'
  import { attention } from './lib/attention'
  import { stashToken, tokenFromHash } from './lib/auth'
  import { approvalFromHash, stashApproval, stashedApproval } from './lib/device'
  import { clock } from './lib/clock.svelte'
  import { servingBanner } from './lib/nodes'
  import { hubLight, nodeLight, runningHere } from './lib/railStatus'
  import { remedy } from './lib/refusal'
  import { router } from './lib/router.svelte'
  import { screenFor } from './lib/routes'
  import { store } from './lib/store.svelte'
  import { surface } from './lib/surface.svelte'
  import { staleInterface, versionLabel } from './lib/version'

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
  // A device approval's address (`#approve=<id>`), opened from a window that
  // cannot make a passkey: the same treatment. Held through a login, then
  // shown in place of the interface until it is decided or closed.
  {
    const a = approvalFromHash(location.hash)
    if (a) {
      stashApproval(a)
      history.replaceState(null, '', location.pathname + location.search)
    }
  }
  let approving = $state(stashedApproval())
  function doneApproving() {
    stashApproval(null)
    approving = null
  }
  // An approval link opened in a tab already showing the interface changes
  // only the fragment, so nothing above runs again.
  window.addEventListener('hashchange', () => {
    const a = approvalFromHash(location.hash)
    if (!a) return
    stashApproval(a)
    approving = a
    history.replaceState(history.state, '', location.pathname + location.search)
  })

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

  // Actionable human decisions only. A review the agent is revising and a
  // publication in flight are still on the home, in their own lanes; counting
  // them here would make the badge a number nobody can act on.
  const waiting = $derived(
    attention({
      questions: store.questions,
      permissions: store.queue.waiting,
      reviews: store.queue.reviews,
      issues: store.issues,
      promotions: store.queue.promotions,
      offers: store.offers,
      nodes: store.nodes,
      mesh: store.mesh,
      now: clock.now,
    }).count,
  )
  const screen = $derived(screenFor(router.path))
  /** Which rail entry the screen belongs to. Enrolling a node is part of Mesh. */
  const nav = $derived(
    screen.name === 'settings' || screen.name === 'enroll'
      ? 'settings'
      : router.path.startsWith('/docs')
        ? 'docs'
        : router.path.startsWith('/memories')
          ? 'memories'
          : router.path.startsWith('/tasks')
            ? 'tasks'
            : router.path.startsWith('/sessions')
              ? 'sessions'
              : router.path === '/usage'
                ? 'usage'
                : router.path === '/more'
                  ? 'more'
                  : 'home'
  )
  /** The phone's More tab stands for every entry its bar has no room for. */
  const more = $derived(['memories', 'usage', 'settings', 'more'].includes(nav))
  const cannotRun = $derived(servingBanner(store.nodes))
  const hubDown = $derived(store.mesh?.hub.state === 'unreachable')
  const staleVersion = $derived(
    staleInterface(
      { version: __TRACON_VERSION__, build: __TRACON_BUILD__ },
      { version: store.node?.application_version, build: store.node?.interface_build },
    )
      ? versionLabel(store.node?.application_version, store.node?.application_build)
      : null,
  )
  const nodeStatus = $derived(
    nodeLight({
      connected: !store.connectionLost,
      node: store.node,
      cannotRun: cannotRun !== null,
      waiting,
      running: runningHere(store.sessions.values(), store.node?.id),
      awake: store.awake,
      now: clock.now,
    }),
  )
  const hubStatus = $derived(hubLight(store.mesh, clock.now))
</script>

{#if store.authRequired}
  <Login />
{:else if approving}
  <DeviceApproval id={approving} ondone={doneApproving} />
{:else if screen.name === 'doc-preview'}
  <DocPreview channel={screen.channel} slug={screen.slug} />
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
    <a href="/tasks" class:on={nav === 'tasks'}>
      <svg viewBox="0 0 24 24"><path d="M5 7h3M5 12h3M5 17h3" /><path d="M11 7h8M11 12h8M11 17h5" /></svg>
      <span class="lbl">Tasks</span>
    </a>
    <a href="/sessions" class:on={nav === 'sessions'}>
      <svg viewBox="0 0 24 24"><rect x="4" y="5" width="16" height="14" rx="2" /><path d="M8 10l3 2-3 2M13 14h3" /></svg>
      <span class="lbl">Sessions</span>
    </a>
    <a href="/docs" class:on={nav === 'docs'}>
      <svg viewBox="0 0 24 24"><path d="M6 3h9l4 4v14H6z" /><path d="M9 11h7M9 15h7M9 7h3" /></svg>
      <span class="lbl">Documents</span>
    </a>
    <a href="/memories" class:on={nav === 'memories'}>
      <svg viewBox="0 0 24 24"><path d="M12 3c-4 2-7 5-7 9a7 7 0 0 0 14 0c0-4-3-7-7-9z" /><path d="M12 8v8M9 12h6" /></svg>
      <span class="lbl">Memories</span>
    </a>
    <a href="/usage" class:on={nav === 'usage'}>
      <svg viewBox="0 0 24 24"><path d="M5 19V11M10 19V5M15 19v-6M20 19V8" /></svg>
      <span class="lbl">Usage</span>
    </a>
    <span class="sp"></span>
    <a href="/settings" class:on={nav === 'settings'}>
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3" /><path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1" /></svg>
      <span class="lbl">Settings</span>
    </a>
    <!-- The name is in Settings; a long one would wrap here. What is wrong is
         in the banners; the light only says that something is. -->
    <div class="foot">
      <span class="light" title={[`node: ${nodeStatus.state}`, ...nodeStatus.detail].join('\n')} aria-label="node: {nodeStatus.state}"
        ><i class="dot {nodeStatus.tone}"></i><span class="lbl">node</span></span
      >
      {#if hubStatus}
        <span class="light" title="hub: {hubStatus.state}" aria-label="hub: {hubStatus.state}"
          ><i class="dot {hubStatus.tone}"></i><span class="lbl">hub</span></span
        >
      {/if}
    </div>
    <button class="collapse" type="button" onclick={toggleRail} title="Collapse">
      <svg viewBox="0 0 24 24"><path d="M15 6l-6 6 6 6" /></svg>
      <span class="lbl">Collapse</span>
    </button>
  </nav>

  <main>
    {#if store.connectionLost}
      <div class="banner crit">Connection lost <b>· reconnecting. Saved work remains on the node; keep this page open to preserve unsent text.</b></div>
    {/if}
    {#if store.node?.state === 'refused'}
      <div class="banner crit">
        Local tasks unavailable <b>· this node failed its isolation check.</b>
        <a href="/settings#system">Review setup</a>
        <details>
          <summary>Technical details</summary>
          <p>{store.node.failed_check}: {store.node.failed_detail}</p>
          <p>{remedy(store.node.failed_check)}</p>
        </details>
      </div>
    {/if}
    {#if cannotRun}
      <div class="banner">This node cannot run tasks <b>· {cannotRun.detail}</b> <a href={cannotRun.href}>{cannotRun.action}</a></div>
    {/if}
    {#if staleVersion}
      <div class="banner">
        tracon {staleVersion} is running <b>· this page is still {versionLabel(__TRACON_VERSION__, __TRACON_BUILD__)}.</b>
        <button class="lnk" type="button" onclick={() => location.reload()}>Reload</button>
      </div>
    {/if}
    <RestartBanner />
    <!-- Degraded is a state, not an error: quiet, persistent, not dismissable. -->
    {#if hubDown}
      <div class="banner dim">Hub unavailable <b>· local sessions continue. Queued shared updates are not yet confirmed on peers.</b></div>
    {:else if store.reconnected !== null}
      <div class="banner ok">Hub reconnected <b>· {store.reconnected} queued update{store.reconnected === 1 ? '' : 's'} handed to the hub</b></div>
    {/if}
    {#if screen.name === 'review'}
      <Review id={screen.id} />
    {:else if screen.name === 'approval'}
      <Approval id={screen.id} />
    {:else if screen.name === 'promotion'}
      <Promotion id={screen.id} />
    {:else if screen.name === 'session'}
      <Session id={screen.id} />
    {:else if screen.name === 'sessions'}
      <Sessions />
    {:else if screen.name === 'enroll'}
      <Enroll />
    {:else if screen.name === 'usage'}
      <Metrics />
    {:else if screen.name === 'task'}
      <WorkItem id={screen.id} />
    {:else if screen.name === 'tasks'}
      <Work />
    {:else if screen.name === 'more'}
      <More />
    {:else if screen.name === 'doc'}
      <Doc channel={screen.channel} slug={screen.slug} edit={screen.edit} />
    {:else if screen.name === 'docs'}
      <Docs />
    {:else if screen.name === 'memories'}
      <Memories />
    {:else if screen.name === 'settings'}
      <Settings />
    {:else}
      <Home />
    {/if}
  </main>

  <!-- Bottom tabs keep every primary destination within thumb reach. -->
  <nav class="tabs">
    <a href="/" class:on={nav === 'home'}>
      <svg viewBox="0 0 24 24"><path d="M4 6h16M4 12h16M4 18h10" /></svg>
      <span>Home</span>
      {#if waiting > 0}<i class="dot"></i>{/if}
    </a>
    <a href="/tasks" class:on={nav === 'tasks'}>
      <svg viewBox="0 0 24 24"><path d="M5 7h3M5 12h3M5 17h3" /><path d="M11 7h8M11 12h8M11 17h5" /></svg>
      <span>Tasks</span>
    </a>
    <a href="/sessions" class:on={nav === 'sessions'}>
      <svg viewBox="0 0 24 24"><rect x="4" y="5" width="16" height="14" rx="2" /><path d="M8 10l3 2-3 2M13 14h3" /></svg>
      <span>Sessions</span>
    </a>
    <a href="/docs" class:on={nav === 'docs'}>
      <svg viewBox="0 0 24 24"><path d="M6 3h9l4 4v14H6z" /><path d="M9 11h7M9 15h7M9 7h3" /></svg>
      <span>Documents</span>
    </a>
    <a href="/more" class:on={more}>
      <svg viewBox="0 0 24 24"><circle cx="6" cy="12" r="1.3" /><circle cx="12" cy="12" r="1.3" /><circle cx="18" cy="12" r="1.3" /></svg>
      <span>More</span>
    </a>
  </nav>
</div>
{/if}
<!-- Above every screen: the passkey a remote admin action waits on, and the
     offer to add one right after pairing. -->
<StepUpPrompt />
<PasskeyOffer />

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
  /* Sits at the bottom because `sp` above it grows; this must not. */
  .foot { padding: 8px 16px; font: 11.5px var(--mono); color: var(--dim); display: flex; flex-direction: column; gap: 3px; min-width: 0; flex: none; }
  /* The dot centred under the nav icons, the word under their labels. */
  .light { display: flex; align-items: center; gap: 15px; padding-left: 5px; cursor: default; }
  .light .dot { width: 7px; height: 7px; border-radius: 50%; flex: none; background: var(--dim); opacity: 0.6; }
  .light .dot.ok { background: var(--ok); opacity: 1; }
  .light .dot.wait { background: var(--wait); opacity: 1; }
  .light .dot.crit { background: var(--crit); opacity: 1; }
  .rail button { color: var(--dim); }
  .rail button:hover { color: var(--ink); }
  .narrow .lbl { display: none; }
  .narrow .light { justify-content: center; padding-left: 0; }
  .narrow .foot { padding-left: 0; padding-right: 0; }
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
      grid-template-columns: repeat(5, minmax(0, 1fr));
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
      min-height: 56px;
      padding: 6px 0 7px;
      color: var(--dim);
      text-decoration: none;
      font: 500 11px/1.2 var(--sans);
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
