<script lang="ts">
  import { onMount } from 'svelte'
  import { shortHash } from '../lib/names'
  import Name from '../components/Name.svelte'
  // What the operator opens onto: a place to start work, then what is waiting
  // on them, then what is running, then what landed. The queue that used to be
  // here showed two empty boxes above a wall of ended sessions — true, and no
  // use. Starting something is the first thing on the page instead.
  import Composer from '../components/Composer.svelte'
  import ExternalAgents from '../components/ExternalAgents.svelte'
  import FirstTaskGuide from '../components/FirstTaskGuide.svelte'
  import { LoadGate } from '@tracon/ui/svelte'
  import OperatorIssueCard from '../components/OperatorIssueCard.svelte'
  import OperatorQuestionCard from '../components/OperatorQuestionCard.svelte'
  import PermissionCard from '../components/PermissionCard.svelte'
  import PromotionCard from '../components/PromotionCard.svelte'
  import RecipeOfferCard from '../components/RecipeOfferCard.svelte'
  import ReviewCard from '../components/ReviewCard.svelte'
  import SessionRow from '../components/SessionRow.svelte'
  import SetupCard from '../components/SetupCard.svelte'
  import SignInBanner from '../components/SignInBanner.svelte'
  import { api } from '../lib/api'
  import { feed, notRunLine, NOT_RUN_SHOWN } from '../lib/allowed-outcome'
  import { attention, type Thread } from '../lib/attention'
  import { defaultChannel, rememberedChannel } from '../lib/channel'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { combineLoads, hasAnswer, loadPhase } from '../lib/load'
  import { eligibleNodes, modelsForChannel } from '../lib/nodes'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { Session, WorkView } from '../lib/types'

  const running = $derived(store.queue.running)
  // The home shows what landed lately; the whole history has its own screen.
  const landed = $derived(store.queue.ended.slice(0, 6))
  // One classification for the whole page, and the same one the rail counts.
  // Everything waiting is here; the lanes say who it is waiting on.
  const bay = $derived(
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
    }),
  )
  const refreshInterventions = () => void store.refreshInterventions()
  // Allowed calls that did not run, until each is opened: an operator who
  // left before the run ended learns it here as well as from the toast.
  const notRun = $derived(
    feed(
      (store.queue.failed ?? []).map((f) => ({ ...f, at_ms: f.finished_ms ?? 0 })),
      store.settled,
      store.seenOutcomes,
      NOT_RUN_SHOWN,
    ),
  )
  let notifications = $state<{ notification_id: string; attempts: { device_id: string; outcome: string }[] }[]>([])
  onMount(() => {
    const refresh = () =>
      api
        .operatorNotifications()
        .then((d) => (notifications = d.notifications))
        .catch(() => {})
    void refresh()
    const timer = setInterval(refresh, 5000)
    return () => clearInterval(timer)
  })
  // A browser can control a ready peer without configuring a local model.
  // Eligibility includes channel membership and every node readiness guard.
  const openChannels = $derived(store.channels.filter((channel) => !channel.archived))
  const memberships = $derived(Object.fromEntries(store.channels.map((channel) => [channel.name, channel.nodes])))
  const taskTargets = $derived.by(() => {
    const preferredChannel = defaultChannel({
      names: openChannels.map((channel) => channel.name),
      remembered: rememberedChannel(),
      nodeDefault: store.node?.default_channel,
      sessions: [...store.sessions.values()].filter((session) => session.node_id === store.node?.id),
    })
    return openChannels
      .flatMap((channel) =>
        eligibleNodes(store.nodes, memberships, channel.name)
          .filter((node) => modelsForChannel(node, channel.name, store.providers, channel.bindings).length > 0)
          .map((node) => ({ node, channel: channel.name })),
      )
      .sort((a, b) => {
        if (a.node.is_self !== b.node.is_self) return a.node.is_self ? -1 : 1
        const byName = a.node.name.localeCompare(b.node.name)
        if (byName !== 0) return byName
        const aPreferred = a.channel === preferredChannel
        const bPreferred = b.channel === preferredChannel
        if (aPreferred !== bPreferred) return aPreferred ? -1 : 1
        return a.channel.localeCompare(b.channel)
      })
  })
  const localTaskTargets = $derived(taskTargets.filter((target) => target.node.is_self))
  const peerTaskTargets = $derived(taskTargets.filter((target) => !target.node.is_self))
  const fallbackTarget = $derived(localTaskTargets[0] ?? peerTaskTargets[0] ?? null)
  let selectedNodeId = $state<string | null>(null)
  let selectedChannel = $state<string | null>(null)
  const selectedTarget = $derived(
    selectedNodeId !== null && selectedChannel !== null
      ? (taskTargets.find((target) => target.node.id === selectedNodeId && target.channel === selectedChannel) ?? null)
      : fallbackTarget,
  )
  const composerNodeId = $derived(selectedTarget?.node.id ?? null)
  const composerChannel = $derived(selectedTarget?.channel ?? null)
  const composerChannelInfo = $derived(store.channels.find((channel) => channel.name === composerChannel) ?? null)
  const canCompose = $derived(taskTargets.length > 0)
  // Until these answer, no task target only means nothing is known yet: the
  // setup checklist would tell a configured node it needs setting up.
  // Providers may fail on their own (a node serving none still works), so
  // only their first answer is waited on.
  const known = $derived(
    combineLoads(store.loads.nodes, store.loads.channels, {
      loaded: store.loads.providers.loaded || store.loads.providers.error !== null,
      error: null,
    }),
  )
  const refetch = () => void store.refetch()
  // A build can ship newer working agreements than the signed bundle the node
  // runs; nothing else says so until a read the newer one allows asks.
  const policyBehind = $derived.by(() => {
    const policy = store.node?.policy
    return policy?.trusted && policy.shipped > policy.running ? policy : null
  })
  function selectTask(nodeId: string, channel: string) {
    selectedNodeId = nodeId
    selectedChannel = channel
  }
  function syncComposerTarget(target: { nodeId: string | null; channel: string }) {
    selectedNodeId = target.nodeId
    selectedChannel = target.nodeId === null ? null : target.channel
  }


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

<!-- A refresh that fails once the node is known keeps the composer, and
     whatever is typed in it, under the banner. -->
{#if loadPhase(known) !== 'ready'}
  <LoadGate status={known} what="this node" onretry={refetch} retrying={store.refetching} />
{/if}
<SignInBanner />
{#if policyBehind}
  <div class="banner">This build ships working agreements v{policyBehind.shipped} <b>· v{policyBehind.running} is installed and still decides requests.</b> <a href="/settings#policies">Review and apply</a></div>
{/if}
{#if known.loaded}
  {#if canCompose}
    {#if !itemId && hasAnswer(store.loads.sessions) && store.sessions.size === 0}
      <FirstTaskGuide
        local={store.node}
        localChannels={localTaskTargets.map((target) => target.channel)}
        peers={peerTaskTargets}
        selectedNodeId={composerNodeId}
        selectedChannel={composerChannel}
        channel={composerChannelInfo}
        onchoose={selectTask}
      />
    {/if}
    {#key itemId}
      <Composer {item} {phase} preferredNodeId={composerNodeId} preferredChannel={composerChannel} ontargetchange={syncComposerTarget} />
    {/key}
  {:else}
    <SetupCard />
  {/if}
{/if}

{#snippet card(thread: Thread)}
  {#if thread.kind === 'question'}
    <OperatorQuestionCard question={thread.question} done={refreshInterventions} />
  {:else if thread.kind === 'permission'}
    <PermissionCard permission={thread.permission} />
  {:else if thread.kind === 'review'}
    <ReviewCard review={thread.review} />
  {:else if thread.kind === 'issue'}
    <OperatorIssueCard issue={thread.issue} done={refreshInterventions} />
  {:else if thread.kind === 'offer'}
    <RecipeOfferCard offer={thread.offer} />
  {:else}
    <PromotionCard promotion={thread.promotion} />
  {/if}
{/snippet}

{#snippet lane(threads: Thread[], heading: string, note: string)}
  {#if threads.length}
    <div class="h4">{heading} <b>{threads.length} · {note}</b></div>
    <div class="rows">
      {#each threads as thread (thread.key)}
        <div class="thread">
          {#if thread.reason}<span class="why">{thread.reason}</span>{/if}
          {@render card(thread)}
        </div>
      {/each}
    </div>
  {/if}
{/snippet}

<!-- A failed queue leaves the lanes below short of every permission, review
     and promotion; saying nothing would read as nothing waiting. A node that
     cannot be reached already says so above, with the same Retry. -->
{#if store.loads.queue.error !== null && loadPhase(known) === 'ready'}
  <LoadGate status={store.loads.queue} what="what is waiting on you" onretry={refetch} retrying={store.refetching} />
{/if}

{@render lane(bay.decisions, 'Waiting on you', 'questions and requests before reviews · oldest first')}
{@render lane(bay.agent, 'With the agent', 'nothing to decide until it comes back')}
{@render lane(bay.external, 'Outside this node', 'in flight, or waiting on something to return')}

{#if notRun.length}
  <div class="h4">Allowed, didn't run <b>{notRun.length} · until you open it</b></div>
  <div class="rows">
    {#each notRun as o (o.id)}
      <a class="notrun" href="/approvals/{o.id}">
        <span>{notRunLine(o)}</span>
        <span class="mono">{formatAge(o.at_ms, clock.now)} ago</span>
      </a>
    {/each}
  </div>
{/if}

{#if notifications.length}
  <details class="deliveries">
    <summary>Notification delivery attempts</summary>
    <p>Delivery attempts only.</p>
    {#each notifications as notification (notification.notification_id)}
      <div class="mono"><Name id={notification.notification_id} label={shortHash(notification.notification_id)} /> · {#each notification.attempts as attempt, i (i)}{i ? ', ' : ''}<Name id={attempt.device_id} label={shortHash(attempt.device_id)} />: {attempt.outcome}{:else}no matching live device{/each}</div>
    {/each}
  </details>
{/if}

{#if running.length}
  <div class="h4">Running <b>{running.length}</b></div>
  <div class="rows">
    {#each running as s (s.id)}
      <SessionRow session={s} />
    {/each}
  </div>
{/if}

<ExternalAgents />

{#if landed.length}
  <div class="h4">
    Landed recently <b>{landed.length}</b>
    <span class="r">
      <button class="lnk" onclick={archiveAll} disabled={busy}>{busy ? 'Archiving…' : 'archive all'}</button>
      <a href="/sessions">all sessions</a>
      <a href="/reviews">all reviews</a>
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
  /* A thread off the decision lane keeps its card and says why it is there;
     nothing is hidden to make the count smaller. */
  .thread {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .thread .why {
    font: 11.5px var(--mono);
    color: var(--dim);
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
  .notrun {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 12px;
    background: var(--s1);
    box-shadow: inset 2px 0 0 var(--crit);
    color: var(--ink);
    text-decoration: none;
    font: 13px var(--sans);
  }
  .notrun .mono {
    color: var(--dim);
    font: 11.5px var(--mono);
    flex: none;
  }
</style>
