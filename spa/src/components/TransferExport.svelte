<script lang="ts">
  import { api } from '../lib/api'
  import { generatedName, nodeDisplayName } from '../lib/names'
  import { autogrow } from '../lib/autogrow'
  import { store } from '../lib/store.svelte'
  import Name from './Name.svelte'
  import type { Session } from '../lib/types'

  // With a session, the panel also forks it: a new session on a branch of
  // its own from this one's, here or on another node, while this one carries
  // on with its own task.
  let { channel, session }: { channel: string; session?: Session } = $props()

  const selfId = $derived(store.nodes.find((node) => node.is_self)?.id ?? null)
  // A fork is made where the session runs, from its own workspace; an
  // external session's checkout and a reviewer's are not ones to fork.
  const forkable = $derived(
    !!session &&
      session.harness_id !== 'external' &&
      session.phase !== 'review' &&
      (selfId === null || session.node_id === selfId),
  )
  let mode = $state<'move' | 'fork'>('move')

  let candidateId = $state('')
  let destination = $state('')
  let documentSlugs = $state('')
  let memoryIds = $state('')
  let note = $state('')
  // A move takes the candidate's session over: the receiver's session holds
  // its task, project and budget, and this one closes once that import lands.
  let moving = $state(true)
  let sending = $state(false)
  let result = $state<string | null>(null)
  let error = $state<string | null>(null)

  // The branch a fork gets unless renamed: this one's with `-fork`, numbered
  // when another session on the channel has it already (as the node does).
  function defaultForkBranch(source: Session): string {
    const taken = new Set([...store.sessions.values()].filter((s) => s.channel === source.channel).map((s) => s.branch))
    const stem = `${source.branch}-fork`
    if (!taken.has(stem)) return stem
    for (let n = 2; n < 1000; n++) if (!taken.has(`${stem}-${n}`)) return `${stem}-${n}`
    return `${stem}-${Date.now()}`
  }
  let forkBranch = $state('')
  let forkBranchFor = $state<string | null>(null)
  $effect(() => {
    if (session && forkBranchFor !== session.id) {
      forkBranchFor = session.id
      forkBranch = defaultForkBranch(session)
    }
  })
  let forkModel = $state('')
  let forkBudget = $state('')
  let forkTask = $state<'none' | 'new' | 'existing'>('none')
  let forkTaskTitle = $state('')
  let forkTaskId = $state('')
  let forked = $state<{ id: string } | null>(null)

  async function forkSession() {
    if (!session || sending || !forkBranch.trim()) return
    sending = true
    error = null
    result = null
    forked = null
    try {
      const budget = forkBudget.trim() ? Number(forkBudget.trim()) : undefined
      if (budget !== undefined && (!Number.isFinite(budget) || budget < 0)) throw new Error('The budget is a number of tokens, 0 or more.')
      const response = await api.forkSession(session.id, {
        destination_node: destination || undefined,
        branch: forkBranch.trim(),
        note,
        documents: ids(documentSlugs),
        memories: ids(memoryIds),
        model: forkModel.trim() || undefined,
        budget_tokens: budget,
        work_item_id: forkTask === 'existing' && forkTaskId.trim() ? forkTaskId.trim() : undefined,
        new_item: forkTask === 'new' && forkTaskTitle.trim() ? { title: forkTaskTitle.trim() } : undefined,
      })
      if (response.delivery === 'local') {
        forked = { id: response.session.id }
        result = `Forked at ${response.head_sha.slice(0, 12)} on ${response.branch}. This session carries on.`
      } else {
        result = `Fork queued for ${nodeDisplayName(store.nodes, response.destination_node)} on ${response.branch}, at ${response.head_sha.slice(0, 12)}. It starts there once the package arrives; this session carries on.`
      }
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      sending = false
    }
  }

  const destinations = $derived(
    store.nodes.filter((node) => !node.is_self && node.reachable && store.channels.some((c) => c.name === channel && c.nodes.includes(node.id))),
  )

  function ids(value: string): string[] {
    return value
      .split(',')
      .map((part) => part.trim())
      .filter(Boolean)
  }

  function download(value: unknown, name: string) {
    const blob = new Blob([JSON.stringify(value, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const link = document.createElement('a')
    link.href = url
    link.download = name
    link.click()
    URL.revokeObjectURL(url)
  }

  async function exportTransfer() {
    if (!candidateId.trim() || sending) return
    sending = true
    error = null
    result = null
    try {
      const response = await api.exportTransfer({
        candidate_id: candidateId.trim(),
        context: { documents: ids(documentSlugs), memories: ids(memoryIds), note },
        destination_node: destination || undefined,
        // A selected node means direct sealed mesh delivery, in pieces when the
        // package is larger than one frame. The server refuses an unavailable
        // online transfer rather than downloading or importing somewhere else.
        delivery: destination ? 'mesh' : 'portable',
        intent: moving ? 'move' : 'handoff',
      })
      if (response.delivery === 'portable') {
        download(response.transfer, `tracon-${generatedName('transfer', response.transfer.payload.candidate_id)}.json`)
        result = moving
          ? 'Signed package downloaded. Import it on the receiving node; this session closes once that node confirms the import over the mesh.'
          : 'Signed package downloaded. On the receiving node, choose Import a session and explicitly start the isolated continuation.'
      } else {
        result = moving
          ? `Sealed package queued for ${nodeDisplayName(store.nodes, destination)}. Once that node imports it, its new session takes this work over and this session closes.`
          : `Sealed package queued for ${nodeDisplayName(store.nodes, destination)}. That node must explicitly import it before a new isolated session starts.`
      }
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      sending = false
    }
  }
</script>

<details class="transfer">
  <summary>{forkable ? 'Move or fork' : 'Move to another node'}</summary>
  {#if forkable}
    <div class="modes" role="radiogroup" aria-label="Move or fork">
      <label class="check"><input type="radio" bind:group={mode} value="move" /><span>Move: the work carries on elsewhere and this session closes</span></label>
      <label class="check"><input type="radio" bind:group={mode} value="fork" /><span>Fork: a new session from this one's branch and context, while this one keeps running</span></label>
    </div>
  {/if}
  {#if forkable && mode === 'fork' && session}
  <p class="consequence">
    The fork gets its own branch, workspace, budget and model. This session keeps its task, reviews and branch. Only committed work goes with it.
  </p>
  <label>
    <span>Where</span>
    <select bind:value={destination}>
      <option value="">This node</option>
      {#each destinations as node (node.id)}
        <option value={node.id}>{nodeDisplayName(store.nodes, node.id)}</option>
      {/each}
    </select>
    <small>This node, or a reachable node in {channel}.</small>
  </label>
  <label>
    <span>Fork's branch</span>
    <input bind:value={forkBranch} autocapitalize="off" autocomplete="off" spellcheck="false" />
    <small>From {session.branch}.</small>
  </label>
  <label>
    <span>Model</span>
    <input bind:value={forkModel} placeholder={session.model} autocapitalize="off" autocomplete="off" />
    <small>Blank keeps this session's.</small>
  </label>
  <label>
    <span>Budget (tokens)</span>
    <input bind:value={forkBudget} inputmode="numeric" placeholder={String(session.budget_tokens)} />
    <small>Blank keeps this session's.</small>
  </label>
  <label>
    <span>Task</span>
    <select bind:value={forkTask}>
      <option value="none">None</option>
      <option value="new">A new task</option>
      <option value="existing">Another existing task</option>
    </select>
    {#if session.work_item_id}<small>This session keeps its own task.</small>{/if}
  </label>
  {#if forkTask === 'new'}
    <label>
      <span>New task title</span>
      <input bind:value={forkTaskTitle} placeholder="What the fork works on" />
    </label>
  {:else if forkTask === 'existing'}
    <label>
      <span>Task ID</span>
      <input bind:value={forkTaskId} placeholder="Task ID" autocapitalize="off" autocomplete="off" />
    </label>
  {/if}
  <label>
    <span>Included document slugs</span>
    <small>Comma separated; Markdown only.</small>
    <input bind:value={documentSlugs} placeholder="guide-release, ref-api" />
  </label>
  <label>
    <span>Included memory IDs</span>
    <small>Comma separated.</small>
    <input bind:value={memoryIds} placeholder="Memory IDs" />
  </label>
  <label>
    <span>Note for the fork</span>
    <textarea class="grow" bind:value={note} use:autogrow={note} placeholder="Try another approach, or the follow-up to split off"></textarea>
  </label>
  <button
    class="btn"
    type="button"
    onclick={forkSession}
    disabled={sending || !forkBranch.trim() || (forkTask === 'new' && !forkTaskTitle.trim()) || (forkTask === 'existing' && !forkTaskId.trim())}
  >
    {sending ? 'Forking…' : destination ? 'Send fork' : 'Start fork'}
  </button>
  {#if result}
    <p class="ok">
      {result}
      {#if forked}<Name kind="session" id={forked.id} href="/sessions/{forked.id}" class="lnk" prefix="Open " />{/if}
    </p>
  {/if}
  {#if error}<p class="bad">{error}</p>{/if}
  {:else}
  <p class="consequence">
    Continue a candidate elsewhere, with its branch's commits and only the named context. No credentials travel; the receiver confirms the import.
  </p>
  <label>
    <span>Source candidate ID</span>
    <input bind:value={candidateId} placeholder="Immutable candidate ID" autocapitalize="off" autocomplete="off" />
  </label>
  <label>
    <span>Destination node</span>
    <select bind:value={destination}>
      <option value="">Portable download — choose and import on another node</option>
      {#each destinations as node (node.id)}
        <option value={node.id}>{nodeDisplayName(store.nodes, node.id)}</option>
      {/each}
    </select>
    <small>Reachable nodes in {channel}. Blank downloads a package.</small>
  </label>
  <label class="check">
    <input type="checkbox" bind:checked={moving} />
    <span>Move this work: the new session takes over its task, project and budget, and the candidate's session here closes once the receiver imports.</span>
  </label>
  <label>
    <span>Included document slugs</span>
    <small>Comma separated; Markdown only.</small>
    <input bind:value={documentSlugs} placeholder="guide-release, ref-api" />
  </label>
  <label>
    <span>Included memory IDs</span>
    <small>Comma separated.</small>
    <input bind:value={memoryIds} placeholder="Memory IDs" />
  </label>
  <label>
    <span>Handoff note</span>
    <textarea class="grow" bind:value={note} use:autogrow={note} placeholder="What the next session should know"></textarea>
  </label>
  <button class="btn" type="button" onclick={exportTransfer} disabled={sending || !candidateId.trim()}>
    {sending ? 'Preparing package…' : destination ? 'Send sealed package' : 'Download signed package'}
  </button>
  {#if result}<p class="ok">{result}</p>{/if}
  {#if error}<p class="bad">{error}</p>{/if}
  {/if}
</details>

<style>
  .transfer {
    margin: 14px 0;
    padding: 9px 11px;
    background: var(--s1);
    border-left: 3px solid var(--ink2);
  }
  summary { cursor: pointer; font: 600 13px var(--sans); }
  .consequence, label, .transfer > p {
    color: var(--ink2);
    font: 12px var(--mono);
  }
  .consequence { margin: 4px 0 10px; }
  .modes { display: grid; gap: 2px; margin: 6px 0 4px; }
  .modes label { margin-top: 4px; }
  label {
    display: grid;
    gap: 3px;
    margin-top: 10px;
  }
  label > span {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  small { color: var(--dim); }
  input, select, textarea { width: 100%; }
  label.check {
    display: flex;
    gap: 8px;
    align-items: flex-start;
  }
  label.check > input { width: auto; }
  label.check > span { text-transform: none; letter-spacing: 0; }
  .transfer > .btn { margin-top: 12px; }
  .ok { color: var(--ok); }
  .transfer > p.bad { color: var(--crit); }
</style>
