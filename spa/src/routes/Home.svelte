<script lang="ts">
  import { onMount } from 'svelte'
  // What the operator opens onto: a place to start work, then what is waiting
  // on them, then what is running, then what landed. The queue that used to be
  // here showed two empty boxes above a wall of ended sessions — true, and no
  // use. Starting something is the first thing on the page instead.
  import Composer from '../components/Composer.svelte'
  import OperatorIssueCard from '../components/OperatorIssueCard.svelte'
  import OperatorQuestionCard from '../components/OperatorQuestionCard.svelte'
  import PermissionCard from '../components/PermissionCard.svelte'
  import PromotionCard from '../components/PromotionCard.svelte'
  import ReviewCard from '../components/ReviewCard.svelte'
  import SessionRow from '../components/SessionRow.svelte'
  import SetupCard from '../components/SetupCard.svelte'
  import { api } from '../lib/api'
  import { setupSteps } from '../lib/firstrun'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { OperatorIssue, OperatorQuestion, Session, WorkView } from '../lib/types'

  const waiting = $derived(store.queue.waiting)
  const reviews = $derived(store.queue.reviews)
  const promotions = $derived(store.queue.promotions ?? [])
  const running = $derived(store.queue.running)
  // The home shows what landed lately; the whole history has its own screen.
  const landed = $derived(store.queue.ended.slice(0, 6))
  let questions = $state<OperatorQuestion[]>([])
  let issues = $state<OperatorIssue[]>([])
  let notifications = $state<{ notification_id: string; attempts: { device_id: string; outcome: string }[] }[]>([])
  async function refreshInterventions() {
    const [questionResult, issueResult, notificationResult] = await Promise.allSettled([
      api.operatorQuestions(), api.operatorIssues(), api.operatorNotifications(),
    ])
    if (questionResult.status === 'fulfilled') questions = questionResult.value.questions
    if (issueResult.status === 'fulfilled') issues = issueResult.value.issues
    if (notificationResult.status === 'fulfilled') notifications = notificationResult.value.notifications
  }
  onMount(() => {
    void refreshInterventions()
    const timer = setInterval(() => void refreshInterventions(), 2000)
    return () => clearInterval(timer)
  })
  const ready = $derived(
    setupSteps({
      anyProviderConnected: store.providers.some((p) => p.state === 'connected'),
      anyChannel: store.channels.some((c) => !c.archived),
      boundaryReady: store.node?.state === 'ready',
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

{#if questions.length || waiting.length || reviews.length || promotions.length || issues.some((issue) => issue.state !== 'published') || notifications.length}
  <div class="h4">
    Waiting on you <b>{questions.length + waiting.length + reviews.length + promotions.length + issues.filter((issue) => issue.state !== 'published').length} · questions and requests before reviews · oldest first</b>
  </div>
  <div class="rows">
    {#each questions as question (question.id)}
      <OperatorQuestionCard {question} done={refreshInterventions} />
    {/each}
    {#each waiting as p (p.id)}
      <PermissionCard permission={p} />
    {/each}
    {#each reviews as r (r.id)}
      <ReviewCard review={r} />
    {/each}
    {#each issues.filter((issue) => issue.state !== 'published') as issue (issue.id)}
      <OperatorIssueCard {issue} done={refreshInterventions} />
    {/each}
  {#if notifications.length}
    <details class="deliveries">
      <summary>Notification delivery attempts</summary>
      <p>Attempt and peer-acknowledgement records only; they do not prove a person saw a notification.</p>
      {#each notifications as notification (notification.notification_id)}
        <div class="mono">{notification.notification_id.slice(0, 8)} · {notification.attempts.length ? notification.attempts.map((attempt) => `${attempt.device_id.slice(0, 8)}: ${attempt.outcome}`).join(', ') : 'no matching live device'}</div>
      {/each}
    </details>
  {/if}
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
