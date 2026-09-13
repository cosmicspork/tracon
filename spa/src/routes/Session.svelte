<script lang="ts">
  import Log from '../components/Log.svelte'
  import OperatorQuestionCard from '../components/OperatorQuestionCard.svelte'
  import PermissionCard from '../components/PermissionCard.svelte'
  import TransferExport from '../components/TransferExport.svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { desktopCanOpenOpencode, openOpencodeWindow } from '../lib/desktop-opencode'
  import { draftBox } from '../lib/draft'
  import { humanizeError } from '../lib/errors'
  import { formatAge, formatBudget, formatTokens } from '../lib/format'
  import { repetitionHint } from '../lib/log'
  import { chipLabel, nodeById, unreachableReason } from '../lib/nodes'
  import {
    isTerminal,
    type CeilingInfo,
    type OperatorQuestion,
    type SessionUsage,
    type ToolchainStatus,
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
  let toolchain = $state<ToolchainStatus | null>(null)
  // The box's timing rules live in lib/draft; the component only holds the text.
  const box = draftBox((text) => api.saveDraft(id, text).catch(() => {}))

  const session = $derived(store.sessions.get(id))
  const waiting = $derived(store.waitingFor(id))
  const busy = $derived(session?.turn_active === 1)
  const owner = $derived(session ? nodeById(store.nodes, session.node_id) : undefined)
  const remote = $derived(owner !== undefined && !owner.is_self)
  // A hint, deliberately not a banner state: the session is still running and
  // nothing has been decided for the operator.
  const repeating = $derived(repetitionHint(store.events))
  let questions = $state<OperatorQuestion[]>([])
  async function refreshQuestions() {
    const result = await api.session(id)
    questions = result.questions
    usage = result.usage
    ceiling = result.ceiling
    toolchain = result.toolchain
  }

  /** What the header says about the baked language toolchain. Nothing at all
   *  for a harness that has none; otherwise the languages this session could
   *  actually get a server for, and — loudly — the ones it was promised and
   *  the image did not have. */
  const toolchainNote = $derived.by(() => {
    if (!toolchain) return null
    const named = toolchain.tools.filter((t) => t.state !== 'disabled')
    if (named.length === 0) return null
    const missing = named.filter((t) => t.state === 'unavailable')
    const text = missing.length
      ? `lsp/fmt · ${missing.length} missing`
      : `lsp/fmt · ${named.length} ready`
    return {
      text,
      warn: missing.length > 0,
      title: named
        .map((t) => `${t.kind} ${t.id} ${t.version} ${t.state} (${t.path})`)
        .concat(
          toolchain.image_revision === toolchain.revision
            ? []
            : [`image toolchain revision ${toolchain.image_revision ?? 'none'}, node expects ${toolchain.revision}`],
        )
        .join('\n'),
    }
  })
  const unreachable = $derived(session ? unreachableReason(store.nodes, store.mesh, session.node_id) : null)

  $effect(() => {
    void store.open(id)
    restored = false
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

  // Two sources, one ledger: the gateway's on-the-wire count is what the
  // budget was charged, the harness's own number is shown beside it, and a
  // turn that disagreed or could not be counted says so rather than reading
  // as an ordinary one.
  const usageNote = $derived.by(() => {
    if (!usage || usage.state === null) return null
    const both = `gw ${formatTokens(usage.gateway_tokens)} · harness ${formatTokens(usage.harness_tokens)}`
    if (usage.unmetered_turns > 0)
      return {
        warn: true,
        text: `${both} · ${usage.unmetered_turns} unmetered`,
        title: `${usage.unmetered_turns} turn(s) spent tokens this node could not count: the provider returned no usage. Not zero — unknown.`,
      }
    if (usage.mismatched_turns > 0)
      return {
        warn: true,
        text: `${both} · ${usage.mismatched_turns} mismatched`,
        title: 'The gateway and the harness disagreed on a turn. The budget was charged the gateway count.',
      }
    return { warn: false, text: `${both} · reconciled`, title: 'The two usage sources agree within tolerance.' }
  })

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
    if (session.state === 'paused') return 'paused'
    if (session.state === 'starting') return 'starting'
    if (session.state === 'waiting_on_check') return `running ${checkCommand ?? 'the checks'}`
    if (busy) return 'a turn is running'
    if (session.budget_tokens > 0 && session.tokens_used >= session.budget_tokens) return 'over budget'
    return null
  })
  // A prompt to an unreachable owner is queued on this node and sent when it
  // returns; the box stays open and says so.
  // The desktop app only: OpenCode's own interface opens in a second window
  // that holds none of this one's privileges. It is the session's own harness,
  // so it is offered for an OpenCode session running on this machine and
  // nowhere else.
  const canOpenOpencode = $derived(
    desktopCanOpenOpencode() && session?.harness_id === 'opencode' && !remote,
  )
  let openingOpencode = $state(false)
  async function openOpencode() {
    openingOpencode = true
    error = null
    try {
      await openOpencodeWindow(id)
    } catch (err) {
      // A Tauri command rejects with the sentence the wrapper wrote.
      error = err instanceof Error ? err.message : String(err)
    } finally {
      openingOpencode = false
    }
  }

  const placeholder = $derived(
    unreachable !== null
      ? `${unreachable} — the prompt is sent when it returns`
      : inputReason
        ? `Input disabled: ${inputReason}`
        : 'Send a prompt. Drafts are held on the node.',
  )
</script>

{#if !session}
  <div class="empty">Loading session…</div>
{:else}
  <header class="sess">
    <a class="lnk" href="/">‹ Queue</a>
    <span class="model">{session.harness_id === 'external' ? 'External agent' : session.model}</span>
    <span class="chip">{session.phase}</span>
    <span class="chip" class:self={owner?.is_self} class:off={unreachable !== null}
      >{chipLabel(store.nodes, session.node_id)}{unreachable !== null && owner?.last_seen_ms
        ? ` · last seen ${formatAge(owner.last_seen_ms, clock.now)}`
        : ''}</span
    >
    {#if session.harness_found}
      <span
        class="mono"
        title="the harness this session ran, and the protocol it negotiated (pinned {session.harness_version})"
        >{session.harness_agent ?? session.harness_id} {session.harness_found} ·
        {session.harness_protocol}</span
      >
    {/if}
    {#if toolchainNote}
      <span class="mono" class:unsure={toolchainNote.warn} title={toolchainNote.title}
        >{toolchainNote.text}</span
      >
    {/if}
    {#if session.harness_id !== 'external'}
      <span class="mono">{session.worktree_path ?? session.repo_path}</span>
      <span class="mono">{session.branch}</span>
    {:else}
      <span class="mono" title="The external host and its repository stay outside Tracon's supervision.">brokered external attachment</span>
    {/if}
    <span class="sp"></span>
    {#if session.harness_id === 'external'}
      <span class="mono" title="External-agent model use happens outside Tracon's metered runtime.">usage outside Tracon · unknown</span>
    {:else}
      <span class="mono">{formatBudget(session.tokens_used, session.budget_tokens)} tok</span>
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
    {#if session.policy_version != null}
      <span class="mono">policy v{session.policy_version}</span>
    {/if}
    {#if session.manifest_digest}
      <span
        class="mono"
        title="the launch manifest this session was staged from — its skills, instructions, agents and approved plugins. A later revision is for the next session, not this one."
        >manifest {session.manifest_digest.slice(0, 8)}</span
      >
    {/if}
    {#if session.work_item_id}
      <a class="mono" href="/work/{session.work_item_id}">item {session.work_item_id.slice(0, 8)}</a>
    {/if}
    {#if canOpenOpencode}
      <button
        class="lnk"
        onclick={() => void openOpencode()}
        disabled={openingOpencode}
        title="OpenCode's own interface, in a window that can reach nothing but that interface"
        >OpenCode</button
      >
    {/if}
    {#if !isTerminal(session.state)}
      {#if session.state === 'paused'}
        <button class="lnk" onclick={() => void control('resume')} disabled={unreachable !== null || controlling}
          >{session.harness_id === 'external' ? 'Resume broker access' : 'Resume'}</button
        >
      {:else if session.state !== 'starting'}
        <button class="lnk" onclick={() => void control('pause')} disabled={unreachable !== null || controlling}
          >{session.harness_id === 'external' ? 'Pause broker access' : 'Pause'}</button
        >
      {/if}
      <button class="lnk d" onclick={stop} disabled={unreachable !== null}
        >{confirmingKill
          ? 'Stop — tap again'
          : session.harness_id === 'external'
            ? 'Stop broker access'
            : 'Stop'}</button
      >
      {#if confirmingKill}
        <button class="lnk" onclick={() => (confirmingKill = false)}>Cancel</button>
      {/if}
    {/if}
  </header>

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
  {:else if session.state === 'paused'}
    <div class="banner dim">
      {session.harness_id === 'external' ? 'broker access paused' : 'paused'}
      <b>· {session.harness_id === 'external' ? 'the external host process continues; Tracon cannot control it' : 'new prompts, tools, and model requests are fenced until resume or stop'}</b>
    </div>
  {:else if session.end_reason === 'item_close'}
    <div class="banner ok">ended at item close <b>· the work item is closed{session.work_item_id ? ` · ${session.work_item_id.slice(0, 8)}` : ''}</b></div>
  {:else if session.end_reason === 'phase_done'}
    <div class="banner ok">{session.phase === 'plan' ? 'plan written' : 'verdict given'} <b>· this {session.phase} session is done</b></div>
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

  <Log events={store.events} openChunks={store.openChunks} toolProgress={store.toolProgress} />
  <TransferExport channel={session.channel} />

  {#each questions as question (question.id)}
    <OperatorQuestionCard {question} done={refreshQuestions} />
  {/each}

  {#each waiting as p (p.id)}
    <PermissionCard permission={p} inline />
  {/each}

  {#if error}
    <div class="banner crit">refused <b>· {error}</b></div>
  {/if}
  {#if isTerminal(session.state) && session.draft}
    <div class="banner dim">unsent prompt retained <b>· copy it before starting another session</b><pre>{session.draft}</pre></div>
  {/if}

  {#if restored && !isTerminal(session.state)}
    <div class="banner dim">draft restored <b>· the node kept what you typed; it was never sent</b></div>
  {/if}

  {#if !isTerminal(session.state)}
    <form class="prompt" onsubmit={send}>
      <textarea
        bind:value={draft}
        oninput={onDraftInput}
        {placeholder}
        disabled={inputReason !== null && !busy}
        onkeydown={(e) => {
          if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) void send()
        }}
      ></textarea>
      <button class="btn p" type="submit" disabled={inputReason !== null || !draft.trim()}>
        Send
      </button>
    </form>
  {/if}
{/if}

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
    min-height: 58px;
    resize: vertical;
  }
  textarea:disabled {
    color: var(--dim);
  }
</style>
