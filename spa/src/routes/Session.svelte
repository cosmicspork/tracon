<script lang="ts">
  import { tick } from 'svelte'
  import Log from '../components/Log.svelte'
  import OperatorQuestionCard from '../components/OperatorQuestionCard.svelte'
  import OutcomePanel from '../components/OutcomePanel.svelte'
  import PermissionCard from '../components/PermissionCard.svelte'
  import SessionAuthorityPanel from '../components/SessionAuthorityPanel.svelte'
  import ShownWork from '../components/ShownWork.svelte'
  import ContinuationPanel from '../components/ContinuationPanel.svelte'
  import TransferExport from '../components/TransferExport.svelte'
  import { api } from '../lib/api'
  import { autogrow } from '../lib/autogrow'
  import { clock } from '../lib/clock.svelte'
  import { draftBox } from '../lib/draft'
  import { humanizeError } from '../lib/errors'
  import { errorText, isNotFound } from '../lib/load'
  import { exhaustionNote } from '../lib/exhaustion'
  import { budgetProblem, heldAtBudget, suggestedBudget } from '../lib/budget'
  import { externalAgent, formatAge, formatBudget, formatTokens } from '../lib/format'
  import { usageNote as usageNoteFor } from '../lib/usage'
  import { repetitionHint } from '../lib/log'
  import { composerState, sessionControls } from '../lib/session-actions'
  import { router } from '../lib/router.svelte'
  import { chipLabel, nodeById, unreachableReason } from '../lib/nodes'
  import {
    isTerminal,
    type CeilingInfo,
    type OperatorQuestion,
    type SessionExhaustion,
    type SessionUsage,
    type ShownWork as ShownWorkItem,
  } from '../lib/types'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'

  let { id }: { id: string } = $props()

  let draft = $state('')
  /** Shown once, when text the operator typed elsewhere comes back from the node. */
  let restored = $state(false)
  let sending = $state(false)
  let error = $state<string | null>(null)
  let usage = $state<SessionUsage | null>(null)
  let ceiling = $state<CeilingInfo | null>(null)
  let exhaustion = $state<SessionExhaustion | null>(null)
  const exhausted = $derived(exhaustionNote(exhaustion))
  // The box's timing rules live in lib/draft; the component only holds the text.
  const box = draftBox((text) => api.saveDraft(id, text).catch(() => {}))

  const session = $derived(store.sessions.get(id))
  /** Paused by an exhausted provider and left for the operator: the banner carries Resume. */
  const held = $derived(session?.state === 'paused' && exhaustion?.outcome === 'held')
  /** Held at its token budget: the banner carries the raise, and Resume alone would be refused. */
  const atBudget = $derived(session ? heldAtBudget(session) : false)
  const controls = $derived(session ? sessionControls(session, held || atBudget) : null)
  const waiting = $derived(store.waitingFor(id))
  const busy = $derived(session?.turn_active === 1)
  const owner = $derived(session ? nodeById(store.nodes, session.node_id) : undefined)
  const remote = $derived(owner !== undefined && !owner.is_self)
  // A hint, deliberately not a banner state: the session is still running and
  // nothing has been decided for the operator.
  const repeating = $derived(repetitionHint(store.events))
  let questions = $state<OperatorQuestion[]>([])
  let shownWork = $state<ShownWorkItem[]>([])
  let publications = $state<{ review_id: string; url: string }[]>([])
  /** Why the session's own read failed, while the list has not shown it either. */
  let readError = $state<{ missing: boolean; text: string } | null>(null)
  async function refreshQuestions() {
    let result
    try {
      result = await api.session(id)
    } catch (e) {
      readError = { missing: isNotFound(e), text: errorText(e) }
      throw e
    }
    readError = null
    questions = result.questions
    usage = result.usage
    ceiling = result.ceiling
    exhaustion = result.exhaustion ?? null
    shownWork = result.shown_work ?? []
    publications = result.publications ?? []
  }

  const unreachable = $derived(session ? unreachableReason(store.nodes, store.mesh, session.node_id) : null)

  // The panel renders below the log, and an in-app hash change does not
  // scroll: bring it into view whenever a link opens it.
  const shown = $derived(session !== undefined)
  $effect(() => {
    if (!shown || router.hash !== '#authority') return
    void router.revision
    void tick().then(() => document.getElementById('authority')?.scrollIntoView({ block: 'start', behavior: 'smooth' }))
  })

  $effect(() => {
    void store.open(id)
    restored = false
    readError = null
    budgetInput = null
    editingBudget = false
    // The draft is asked for on its own: it is the one thing a reconnecting
    // client cannot reconstruct, and it should not wait on the rest.
    api
      .draft(id)
      .then((d) => {
        const text = box.restore(d.text)
        if (text !== null) {
          draft = text
          restored = true
        }
      })
      .catch(() => {})
    void refreshQuestions().catch(() => {})
    const timer = setInterval(() => void refreshQuestions().catch(() => {}), 2000)
    return () => {
      clearInterval(timer)
      box.dispose()
      store.close()
    }
  })

  function onDraftInput() {
    // Typing means this box is the operator's now: a late fetch must not
    // overwrite what they are writing.
    restored = false
    box.typed(draft)
  }

  async function send(e?: SubmitEvent) {
    e?.preventDefault()
    const text = draft.trim()
    if (!text || sending) return
    sending = true
    error = null
    restored = false
    // Drop any pending draft save before the prompt clears it on the node; a
    // save firing mid-request would resurrect the sent text into the box.
    box.sent()
    try {
      await api.prompt(id, text)
      draft = ''
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
    } finally {
      sending = false
    }
  }

  const usageNote = $derived(usageNoteFor(usage))

  let controlling = $state(false)
  let confirmingKill = $state(false)

  // The check that is running: the last check_started without a later
  // check_result for its last command, shown with elapsed time.
  const checkStarted = $derived.by(() => {
    const started = [...store.events].reverse().find((e) => e.kind === 'check_started')
    return started ?? null
  })
  const checkCommand = $derived.by(() => {
    const cmds = (checkStarted?.payload.commands as string[] | undefined) ?? []
    const done = store.events.filter((e) => e.kind === 'check_result' && checkStarted && e.seq > checkStarted.seq).length
    return cmds[done] ?? cmds.at(-1) ?? null
  })
  const checkElapsed = $derived(checkStarted ? formatAge(checkStarted.at_ms, clock.now) : '')
  // How many decisions a session held between turns, or put away by a node
  // that stopped under it, is waiting on: what its latest state change named.
  const waitingOn = $derived.by(() => {
    const last = [...store.events].reverse().find((e) => e.kind === 'state')
    const on = last?.payload.waiting_on
    return last?.payload.state === session?.state && Array.isArray(on) ? on.length : 0
  })

  async function stop() {
    // Immediate in the browser; confirmed on the phone, where a stray thumb is
    // likely and the session is someone's work in progress.
    if (surface.phone && !confirmingKill) {
      confirmingKill = true
      return
    }
    confirmingKill = false
    error = null
    try {
      await api.stop(id)
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
    }
  }

  const channelStopped = $derived(
    store.channels.find((c) => c.name === session?.channel)?.bindings?.external_stopped === true,
  )

  async function allowExternal() {
    if (controlling || !session) return
    controlling = true
    error = null
    try {
      await api.externalStart(session.channel)
      await store.refetch()
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
    } finally {
      controlling = false
    }
  }

  // A suspended session's work can be carried on once, as a new session on
  // the same workspace; the node then closes this one. Once it has been, link
  // to that one. An ended session is carried on from the continuation panel,
  // which is the one place its Continue lives.
  const continuedAs = $derived(
    [...store.sessions.values()].find((s) => s.continued_from === id),
  )
  let continuing = $state(false)
  async function carryOn() {
    if (continuing) return
    continuing = true
    error = null
    try {
      const next = await api.continueSession(id)
      await store.refetch()
      router.go(`/sessions/${next.id}`)
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
    } finally {
      continuing = false
    }
  }

  // The raise box: what the operator typed, else the suggestion. Opened from
  // the header's budget on a session that is not held at it.
  let budgetInput = $state<number | null>(null)
  let editingBudget = $state(false)
  let raising = $state(false)
  const budgetValue = $derived(budgetInput ?? (session ? suggestedBudget(session) : 0))
  const budgetOpen = $derived(
    atBudget || (editingBudget && session !== undefined && !isTerminal(session.state) && session.harness_id !== 'external'),
  )
  async function raiseBudget(e?: SubmitEvent) {
    e?.preventDefault()
    if (!session || raising) return
    const problem = budgetProblem(budgetValue, session.tokens_used)
    if (problem) {
      error = `the budget must be ${problem}`
      return
    }
    raising = true
    error = null
    try {
      await api.setBudget(id, budgetValue)
      budgetInput = null
      editingBudget = false
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
    } finally {
      raising = false
    }
  }

  async function control(action: 'pause' | 'resume') {
    if (controlling) return
    controlling = true
    error = null
    try {
      await (action === 'pause' ? api.pause(id) : api.resume(id))
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
    } finally {
      controlling = false
    }
  }

  const inputReason = $derived.by(() => {
    if (!session) return 'loading'
    // The operator is already talking to this one, in its own terminal.
    if (session.harness_id === 'external') return 'a harness you run yourself'
    if (isTerminal(session.state)) return `session ${session.state.replace('_', ' ')}`
    if (atBudget) return 'held at its token budget; raise it to carry on'
    if (session.state === 'paused') return 'paused'
    if (session.state === 'suspended') return 'suspended; continue it to carry on'
    if (session.state === 'starting') return 'starting'
    if (session.state === 'waiting_on_check') return `running ${checkCommand ?? 'the checks'}`
    if (busy) return 'a turn is running'
    if (session.budget_tokens > 0 && session.tokens_used >= session.budget_tokens) return 'over budget'
    return null
  })
  // A prompt to an unreachable owner is queued on this node and sent when it
  // returns; the box stays open and says so.
  // A turn or the checks running: the node holds a prompt for the agent
  // rather than refusing it.
  const composer = $derived(composerState(inputReason, busy || session?.state === 'waiting_on_check'))
  const placeholder = $derived(
    unreachable !== null ? `${unreachable} — the prompt is sent when it returns` : composer.placeholder,
  )
</script>

{#if !session && readError?.missing}
  <div class="banner crit">not found <b>· no session {id.slice(0, 8)}</b> <a class="lnk" href="/sessions">All sessions</a></div>
{:else if !session && readError}
  <div class="banner crit" role="alert">Could not load this session <b>· {readError.text} · trying again</b></div>
{:else if !session}
  <div class="empty">Loading session…</div>
{:else}
  <header class="sess">
    <a class="lnk" href="/">‹ Queue</a>
    <!-- The harness and its protocol are the node's business: on hover only. -->
    <span
      class="model"
      title={session.harness_found
        ? `${session.harness_agent ?? session.harness_id} ${session.harness_found} · ${session.harness_protocol}`
        : undefined}
      >{session.harness_id === 'external' ? ['External agent', externalAgent(session.harness_session_id, session.harness_agent)].filter(Boolean).join(' · ') : session.model}</span
    >
    <span class="chip">{session.phase}</span>
    <span class="chip" class:self={owner?.is_self} class:off={unreachable !== null}
      >{chipLabel(store.nodes, session.node_id)}{unreachable !== null && owner?.last_seen_ms
        ? ` · last seen ${formatAge(owner.last_seen_ms, clock.now)}`
        : ''}</span
    >
    {#if session.harness_id === 'external'}
      <span class="mono" title="The external host and its repository stay outside Tracon's supervision.">brokered external attachment</span>
    {/if}
    <span class="sp"></span>
    {#if session.harness_id === 'external'}
      <span class="mono" title="External-agent model use happens outside Tracon's metered runtime.">usage outside Tracon · unknown</span>
    {:else}
      {#if isTerminal(session.state) || atBudget}
        <span class="mono">{formatBudget(session.tokens_used, session.budget_tokens)} tok</span>
      {:else}
        <button
          class="lnk mono"
          onclick={() => (editingBudget = !editingBudget)}
          disabled={unreachable !== null || remote}
          title={remote ? 'the budget is changed on the node that runs the session' : 'change the token budget'}
          >{formatBudget(session.tokens_used, session.budget_tokens)} tok</button
        >
      {/if}
      {#if usageNote}
        <span class="mono" class:unsure={usageNote.warn} title={usageNote.title}>{usageNote.text}</span>
      {/if}
    {/if}
    {#if session.harness_id !== 'external' && session.context_used != null && session.context_size != null}
      <span class="mono"
        >ctx {formatTokens(session.context_used)}/{formatTokens(session.context_size)}</span
      >
    {/if}
    {#if session.harness_id !== 'external' && session.cost_usd != null}
      <span class="mono">${session.cost_usd.toFixed(2)}</span>
    {/if}
    {#if session.manifest_digest}
      <span
        class="mono"
        title="the launch manifest this session was staged from — its skills, instructions and agents. A later revision is for the next session, not this one."
        >manifest {session.manifest_digest.slice(0, 8)}</span
      >
    {/if}
    <a class="mono" href="#authority" title="where it runs, what it can reach, what it must ask about and what is refused"
      >what it may do</a
    >
    {#if controls?.resume}
      <button class="lnk" onclick={() => void control('resume')} disabled={unreachable !== null || controlling}
        >{session.harness_id === 'external' ? 'Resume broker access' : 'Resume'}</button
      >
    {:else if controls?.pause}
      <button class="lnk" onclick={() => void control('pause')} disabled={unreachable !== null || controlling}
        >{session.harness_id === 'external' ? 'Pause broker access' : 'Pause'}</button
      >
    {/if}
    {#if controls?.stop}
      <button class="lnk d" onclick={stop} disabled={unreachable !== null}
        >{confirmingKill
          ? 'Stop — tap again'
          : session.harness_id === 'external'
            ? `Stop broker access on ${session.channel}`
            : 'Stop'}</button
      >
      {#if confirmingKill}
        <button class="lnk" onclick={() => (confirmingKill = false)}>Cancel</button>
      {/if}
    {/if}
  </header>

  <!-- Everything linked to the work under the title: its task, plan,
       sessions, review, request and branch. Next and its verbs once this
       attempt has ended; until then the header holds the controls. -->
  <ContinuationPanel sessionId={id} actions={isTerminal(session.state)} stamp={`${session.state}:${publications.length}`} />

  {#if session.legacy_ms}
    <div class="banner dim">
      archived · ran <code>{session.harness_id} {session.harness_version}</code>, a harness this
      node no longer has
      <b
        >· readable, never launchable again · its workspace is kept; carry the work forward with
        <code>tracon session reopen {session.id} --harness claude</code></b
      >
    </div>
  {/if}
  {#each publications as p (p.review_id)}
    <div class="banner ok">
      published <a class="lnk" href={p.url} target="_blank" rel="noreferrer">{p.url}</a>
      <b>· from review <a class="lnk" href="/reviews/{p.review_id}">{p.review_id.slice(0, 8)}</a></b>
    </div>
  {/each}
  {#if session.continued_from}
    <div class="banner dim">
      continues <a class="lnk" href="/sessions/{session.continued_from}"
        >{session.continued_from.slice(0, 8)}</a
      >
      <b>· this session inherited that one's workspace, not its context</b>
    </div>
  {/if}

  {#if session.harness_id === 'external' && isTerminal(session.state) && channelStopped}
    <div class="banner dim">
      broker access stopped on {session.channel} <b>· every external agent on it is refused until started again</b>
      <button class="lnk" onclick={allowExternal} disabled={controlling}>Start broker access</button>
    </div>
  {/if}
  {#if session.harness_id === 'external' && session.state !== 'paused' && !isTerminal(session.state)}
    <div class="banner dim">external agent attached <b>· its host process, repository, prompts, and model usage stay outside Tracon; these controls only fence broker access</b></div>
  {/if}

  {#if session.state === 'killed_budget'}
    <div class="banner crit">
      killed at budget <b
        >· {formatTokens(session.tokens_used)} of {formatTokens(session.budget_tokens)} tokens ·
        resume is a new session</b
      >
    </div>
  {:else if session.state === 'failed'}
    <div class="banner crit">failed <b>· {humanizeError(session.last_error) ?? 'the harness stopped without saying why'}</b></div>
  {:else if session.state === 'waiting_on_check'}
    <div class="banner dim">running <code>{checkCommand ?? 'checks'}</code> <b>· {checkElapsed} · input disabled until it finishes</b></div>
  {:else if atBudget}
    <div class="banner">
      held at budget <b
        >· {formatTokens(session.tokens_used)} of {formatTokens(session.budget_tokens)} tokens · its harness and
        context are kept; a higher budget lets it carry on</b
      >
      {@render budgetForm()}
    </div>
  {:else if session.state === 'paused' && exhausted}
    <div class="banner dim">
      paused · {exhausted.title} <b>· {exhausted.detail}</b>
      {#if held}
        <button class="lnk" onclick={() => void control('resume')} disabled={unreachable !== null || controlling}>Resume</button>
      {/if}
    </div>
  {:else if session.state === 'paused'}
    <div class="banner dim">
      {session.harness_id === 'external' ? 'broker access paused' : 'paused'}
      <b>· {session.harness_id === 'external' ? 'the external host process continues; Tracon cannot control it' : 'new prompts, tools, and model requests are fenced until resume or stop'}</b>
    </div>
  {:else if session.state === 'waiting_on_you' && waitingOn > 0}
    <div class="banner dim">
      waiting on you <b
        >· it ended its turn on {waitingOn === 1 ? 'a decision' : `${waitingOn} decisions`} it asked you for; each comes
        back to it as its next prompt</b
      >
    </div>
  {:else if session.state === 'suspended'}
    <div class="banner dim">
      suspended <b
        >· {waitingOn > 0
          ? 'the node stopped while it waited on you; once you have decided, it carries on as a new session'
          : 'its work is published and it went idle; container stopped, egress revoked, workspace kept'}</b
      >
      {#if continuedAs}
        <a class="lnk" href="/sessions/{continuedAs.id}">continued as {continuedAs.id.slice(0, 8)}</a>
      {:else}
        <button class="lnk" onclick={() => void carryOn()} disabled={continuing || unreachable !== null}
          >Continue</button
        >
      {/if}
    </div>
  {:else if session.end_reason === 'continued' && continuedAs}
    <div class="banner dim">
      continued <b>· its work carries on in</b>
      <a class="lnk" href="/sessions/{continuedAs.id}">{continuedAs.id.slice(0, 8)}</a>
    </div>
  {:else if session.end_reason === 'node_restart'}
    <div class="banner dim">
      ended by a node restart <b>· you did not stop it; its workspace is kept</b>
      {#if continuedAs}
        <a class="lnk" href="/sessions/{continuedAs.id}">continued as {continuedAs.id.slice(0, 8)}</a>
      {/if}
    </div>
  {:else if session.end_reason === 'provider_exhausted'}
    <div class="banner dim">
      ended when {exhaustion?.provider ?? 'its provider'} was exhausted <b
        >· {exhaustion?.fallback ? `to carry on with ${exhaustion.fallback}` : 'its workspace is kept'}</b
      >
      {#if continuedAs}
        <a class="lnk" href="/sessions/{continuedAs.id}">continued as {continuedAs.id.slice(0, 8)}</a>
      {/if}
    </div>
  {:else if session.end_reason === 'item_close'}
    <div class="banner ok">ended at task close <b>· the task is closed{session.work_item_id ? ` · ${session.work_item_id.slice(0, 8)}` : ''}</b></div>
  {:else if session.end_reason === 'phase_done'}
    <div class="banner ok">{session.phase === 'plan' ? 'plan written' : 'verdict given'} <b>· this {session.phase} session is done</b></div>
  {/if}
  {#if budgetOpen && !atBudget}
    <div class="banner dim">
      budget <b
        >· {session.budget_tokens > 0
          ? `${formatTokens(session.tokens_used)} of ${formatTokens(session.budget_tokens)} tokens spent`
          : `${formatTokens(session.tokens_used)} tokens spent · no budget`}</b
      >
      {@render budgetForm()}
    </div>
  {/if}
  <!-- Why a running session's turns are failing: the gateway refuses its model
       calls once the channel's counted spend crosses the ceiling. -->
  {#if ceiling?.state === 'at' && !isTerminal(session.state)}
    <div class="banner crit">
      {session.channel} is at its daily ceiling
      <b
        >· {formatTokens(ceiling.usage_today)} of {formatTokens(ceiling.ceiling ?? 0)} tokens{ceiling.unmetered_turns >
        0
          ? `, plus ${ceiling.unmetered_turns} unmetered`
          : ''} · model calls are refused until local midnight or a higher ceiling</b
      >
    </div>
  {/if}
  {#if repeating && !isTerminal(session.state) && session.state !== 'paused'}
    <div class="banner dim">{repeating} <b>· pause it yourself if it is stuck</b></div>
  {/if}
  {#if remote && unreachable !== null}
    <div class="banner dim">{unreachable} <b>· the log resumes when {owner?.name ?? 'it'} returns</b></div>
  {/if}

  <Log events={store.events} openChunks={store.openChunks} toolProgress={store.toolProgress} toolOutput={store.toolOutput} root={session.worktree_path} />
  <!-- Open by default once the session has ended: that is when what it came
       to is the question. -->
  <OutcomePanel {id} open={isTerminal(session.state) || router.hash === '#outcome'} />
  <ShownWork items={shownWork} />
  <TransferExport channel={session.channel} />

  {#each questions as question (question.id)}
    <OperatorQuestionCard {question} done={refreshQuestions} />
  {/each}

  {#each waiting as p (p.id)}
    <PermissionCard permission={p} inline />
  {/each}

  <!-- Not part of the page until asked for: the header's "what it may do" or
       a permission card's "why this is asked" opens it. Keyed on navigation so
       following either link again reopens a panel the operator collapsed. -->
  {#if router.hash === '#authority'}
    {#key router.revision}
      <SessionAuthorityPanel {id} open />
    {/key}
  {/if}

  {#if error}
    <div class="banner crit">refused <b>· {error}</b></div>
  {/if}
  {#if isTerminal(session.state) && session.draft}
    <div class="banner dim">unsent prompt retained <b>· copy it before starting another session</b><pre class="draft">{session.draft}</pre></div>
  {/if}

  {#if restored && !isTerminal(session.state)}
    <div class="banner dim">draft restored <b>· the node kept what you typed; it was never sent</b></div>
  {/if}

  {#if !isTerminal(session.state)}
    <form class="prompt" onsubmit={send}>
      <textarea
        class="grow compact"
        bind:value={draft}
        use:autogrow={draft}
        oninput={onDraftInput}
        {placeholder}
        disabled={composer.disabled}
        onkeydown={(e) => {
          if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) void send()
        }}
      ></textarea>
      <button class="btn p" type="submit" disabled={(inputReason !== null && !composer.queues) || !draft.trim()}>
        {composer.queues ? 'Queue' : 'Send'}
      </button>
    </form>
  {/if}
{/if}

{#snippet budgetForm()}
  <form class="budget" onsubmit={raiseBudget}>
    <input
      type="number"
      min="0"
      step="1"
      value={budgetValue}
      oninput={(e) => (budgetInput = e.currentTarget.valueAsNumber)}
      aria-label="New token budget, 0 for none"
    />
    <span
      >tokens{Number.isFinite(budgetValue)
        ? budgetValue > 0
          ? ` · ${formatTokens(budgetValue)}`
          : ' · no budget'
        : ''}</span
    >
    <button class="lnk" type="submit" disabled={raising || unreachable !== null || remote}
      >{atBudget ? 'Raise budget' : 'Set budget'}</button
    >
  </form>
{/snippet}

<style>
  .sess {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 16px;
    align-items: baseline;
    font: 12.5px var(--mono);
    color: var(--ink2);
    padding-bottom: 12px;
    border-bottom: 1px solid var(--rule);
  }
  .model {
    font: 600 15px var(--sans);
    color: var(--ink);
  }
  /* Usage that did not reconcile, or could not be counted at all. Marked, not
     alarmed: the number is unknown, which is a thing to look at, not a fault. */
  .unsure {
    color: var(--wait);
  }
  .sp {
    flex: 1;
  }
  .prompt {
    display: flex;
    gap: 10px;
    align-items: flex-end;
  }
  textarea {
    flex: 1;
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    font: 13.5px var(--sans);
    padding: 9px 11px;
  }
  /* Plainly not taking input: no resize grip, no text cursor. */
  textarea:disabled {
    color: var(--dim);
    background: transparent;
    outline: 1px dashed var(--rule);
    outline-offset: -1px;
    resize: none;
    cursor: not-allowed;
  }
  .budget {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 10px;
    align-items: baseline;
    margin-top: 6px;
    color: var(--ink2);
  }
  .budget input {
    width: 14ch;
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    font: inherit;
    padding: 3px 6px;
  }
  /* Kept as typed, but wrapped: a long line would widen the page on a phone. */
  .draft {
    margin: 6px 0 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: var(--ink);
    font: inherit;
  }
</style>
