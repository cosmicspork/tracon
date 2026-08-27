<script lang="ts">
  import Log from '../components/Log.svelte'
  import PermissionCard from '../components/PermissionCard.svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge, formatBudget, formatTokens } from '../lib/format'
  import { nodeById, nodeLabel, unreachableReason } from '../lib/nodes'
  import { isTerminal } from '../lib/types'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'

  let { id }: { id: string } = $props()

  let draft = $state('')
  let draftLoaded = $state(false)
  let sending = $state(false)
  let error = $state<string | null>(null)
  let saveTimer: ReturnType<typeof setTimeout> | undefined

  const session = $derived(store.sessions.get(id))
  const waiting = $derived(store.waitingFor(id))
  const busy = $derived(session?.turn_active === 1)
  const owner = $derived(session ? nodeById(store.nodes, session.node_id) : undefined)
  const remote = $derived(owner !== undefined && !owner.is_self)
  const unreachable = $derived(session ? unreachableReason(store.nodes, store.mesh, session.node_id) : null)

  $effect(() => {
    void store.open(id)
    draftLoaded = false
    api
      .session(id)
      .then((d) => {
        // The node holds the draft; a reopened tab resumes with it in the box.
        if (!draftLoaded) draft = d.session.draft ?? ''
        draftLoaded = true
      })
      .catch(() => (draftLoaded = true))
    return () => store.close()
  })

  function onDraftInput() {
    // Typing means this box is the operator's now: a late `api.session` fetch
    // must not overwrite what they are writing.
    draftLoaded = true
    clearTimeout(saveTimer)
    saveTimer = setTimeout(() => void api.saveDraft(id, draft).catch(() => {}), 500)
  }

  async function send(e?: SubmitEvent) {
    e?.preventDefault()
    const text = draft.trim()
    if (!text || sending) return
    sending = true
    error = null
    // Cancel any pending draft save before the prompt clears it on the node; a
    // save firing mid-request would resurrect the sent text into the box.
    clearTimeout(saveTimer)
    try {
      await api.prompt(id, text)
      draft = ''
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
    } finally {
      sending = false
    }
  }

  let confirmingKill = $state(false)

  async function kill() {
    // Immediate in the browser; confirmed on the phone, where a stray thumb is
    // likely and the session is someone's work in progress.
    if (surface.phone && !confirmingKill) {
      confirmingKill = true
      return
    }
    confirmingKill = false
    error = null
    try {
      await api.kill(id)
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
    }
  }

  const inputReason = $derived.by(() => {
    if (!session) return 'loading'
    if (isTerminal(session.state)) return `session ${session.state.replace('_', ' ')}`
    if (session.state === 'starting') return 'starting'
    if (busy) return 'a turn is running'
    if (session.tokens_used >= session.budget_tokens) return 'over budget'
    return null
  })
  // A prompt to an unreachable owner is queued on this node and sent when it
  // returns; the box stays open and says so.
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
    <span class="model">{session.model}</span>
    <span class="chip" class:self={owner?.is_self} class:off={unreachable !== null}
      >{nodeLabel(store.nodes, session.node_id)}{unreachable !== null && owner?.last_seen_ms
        ? ` · last seen ${formatAge(owner.last_seen_ms, clock.now)}`
        : ''}</span
    >
    <span class="mono">{session.worktree_path ?? session.repo_path}</span>
    <span class="mono">{session.branch}</span>
    <span class="sp"></span>
    <span class="mono">{formatBudget(session.tokens_used, session.budget_tokens)} tok</span>
    {#if session.context_used != null && session.context_size != null}
      <span class="mono"
        >ctx {formatTokens(session.context_used)}/{formatTokens(session.context_size)}</span
      >
    {/if}
    {#if session.cost_usd != null}
      <span class="mono">${session.cost_usd.toFixed(2)}</span>
    {/if}
    {#if !isTerminal(session.state)}
      <button class="lnk d" onclick={kill} disabled={unreachable !== null}>{confirmingKill ? 'Kill — tap again' : 'Kill'}</button>
      {#if confirmingKill}
        <button class="lnk" onclick={() => (confirmingKill = false)}>Cancel</button>
      {/if}
    {/if}
  </header>

  {#if session.state === 'killed_budget'}
    <div class="banner crit">
      killed at budget <b
        >· {formatTokens(session.tokens_used)} of {formatTokens(session.budget_tokens)} tokens ·
        resume is a new session</b
      >
    </div>
  {:else if session.state === 'failed'}
    <div class="banner crit">failed <b>· {session.last_error ?? 'harness error'}</b></div>
  {/if}
  {#if remote && unreachable !== null}
    <div class="banner dim">{unreachable} <b>· the log resumes when {owner?.name ?? 'it'} returns</b></div>
  {/if}

  <Log events={store.events} openChunks={store.openChunks} toolProgress={store.toolProgress} />

  {#each waiting as p (p.id)}
    <PermissionCard permission={p} inline />
  {/each}

  {#if error}
    <div class="banner crit">refused <b>· {error}</b></div>
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
