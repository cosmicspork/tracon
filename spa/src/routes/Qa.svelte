<script lang="ts">
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { listCandidates, type EvidenceCandidate, type EvidenceCursor } from '../lib/evidence'
  import { formatAge } from '../lib/format'
  import { nodeLabel } from '../lib/nodes'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { QaEvidence } from '../lib/types'

  type CandidateIdentity = { id: string; owner: string | null; channel: string | null }

  let candidateId = $state('')
  let candidateOwner = $state<string | null>(null)
  let candidateChannel = $state<string | null>(null)
  let evidence = $state<QaEvidence | null>(null)
  let error = $state<string | null>(null)
  let busy = $state<string | null>(null)
  let target = $state('')
  let deploymentId = $state('')
  let startPath = $state('/')
  let expectedPath = $state('/')
  let search = $state('')
  let channel = $state('')
  let runner = $state('')
  let candidates = $state<EvidenceCandidate[]>([])
  let nextBefore = $state<EvidenceCursor | null>(null)
  let browseError = $state<string | null>(null)
  let browseLoaded = $state(false)
  let browsing = $state(false)
  let browseRequest = 0
  let routeCandidate = ''
  let detailRequest = 0
  let actionRequest = 0

  const ordinaryChannels = $derived(store.channels.filter((item) => !item.name.startsWith('@')))
  const channelRunners = $derived(channel ? ordinaryChannels.find((item) => item.name === channel)?.nodes ?? [] : [])
  const evidenceIsLocal = $derived(evidence?.candidate.owner_node_id === store.node?.id)

  function selectedCandidate(): CandidateIdentity {
    return { id: candidateId.trim(), owner: candidateOwner, channel: candidateChannel }
  }

  function identityKey(candidate: CandidateIdentity) {
    return `${candidate.id}\u0000${candidate.owner ?? ''}\u0000${candidate.channel ?? ''}`
  }

  function sameDetail(candidate: CandidateIdentity, request: number) {
    return request === detailRequest && identityKey(selectedCandidate()) === identityKey(candidate)
  }

  function evidenceOwner() {
    return evidence ? nodeLabel(store.nodes, evidence.candidate.owner_node_id) : 'unknown'
  }

  function invalidateActions() {
    actionRequest += 1
    evidence = null
    target = ''
    deploymentId = ''
  }

  function clearDetail() {
    detailRequest += 1
    invalidateActions()
    candidateId = ''
    candidateOwner = null
    candidateChannel = null
    error = null
    busy = null
  }

  async function load(resetSelection = false) {
    const selected = selectedCandidate()
    if (!selected.id) return
    const request = ++detailRequest
    if (resetSelection) invalidateActions()
    busy = 'load'
    error = null
    try {
      const loaded = await api.qaEvidence(selected.id, {
        owner: selected.owner ?? undefined,
        channel: selected.channel ?? undefined,
      })
      if (!sameDetail(selected, request)) return
      evidence = loaded
      if (!target && loaded.targets.length) target = loaded.targets[0].id
      if (!deploymentId && loaded.deployments.length) deploymentId = loaded.deployments[0].id
    } catch (cause) {
      if (!sameDetail(selected, request)) return
      evidence = null
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      if (sameDetail(selected, request)) busy = null
    }
  }

  function actionStillCurrent(action: number, detail: number, candidate: CandidateIdentity) {
    return action === actionRequest
      && detail === detailRequest
      && identityKey(selectedCandidate()) === identityKey(candidate)
      && evidence?.candidate.id === candidate.id
      && evidenceIsLocal
  }

  function sameBrowseRequest(requested: { search: string; channel: string; runner: string }, request: number) {
    return request === browseRequest
      && search === requested.search
      && channel === requested.channel
      && runner === requested.runner
  }

  async function loadCandidates(reset: boolean) {
    if (!reset && !nextBefore) return
    const requested = { search, channel, runner }
    const request = ++browseRequest
    browsing = true
    browseError = null
    if (reset) {
      browseLoaded = false
      candidates = []
      nextBefore = null
    }
    try {
      const page = await listCandidates({
        q: requested.search,
        runner: requested.runner || undefined,
        channel: requested.channel || undefined,
        before: reset ? undefined : nextBefore ?? undefined,
        limit: 16,
      })
      if (!sameBrowseRequest(requested, request)) return
      candidates = reset ? page.items : [...candidates, ...page.items]
      nextBefore = page.next_before
    } catch (cause) {
      if (!sameBrowseRequest(requested, request)) return
      browseError = cause instanceof Error ? cause.message : String(cause)
    } finally {
      if (sameBrowseRequest(requested, request)) {
        browsing = false
        browseLoaded = true
      }
    }
  }

  function resetBrowseCursor() {
    browseRequest += 1
    candidates = []
    nextBefore = null
    browseError = null
    browseLoaded = false
    browsing = false
  }

  function browseChannelChanged() {
    if (runner && !channelRunners.includes(runner)) runner = ''
    resetBrowseCursor()
  }

  function selectCandidate(item: EvidenceCandidate) {
    const selected: CandidateIdentity = {
      id: item.candidate.id.trim(),
      owner: item.candidate.owner_node_id,
      channel: item.candidate.channel,
    }
    if (!selected.id) return
    candidateId = selected.id
    candidateOwner = selected.owner
    candidateChannel = selected.channel
    routeCandidate = identityKey(selected)
    void load(true)
    const query = new URLSearchParams({ candidate: selected.id, channel: selected.channel ?? '' })
    if (selected.owner) query.set('owner', selected.owner)
    const path = `/qa?${query}`
    if (`${router.path}${router.search}` !== path) router.go(path)
  }

  function selectTechnicalCandidate() {
    const id = candidateId.trim()
    if (!id) return
    const selected = { id, owner: null, channel: null }
    candidateOwner = null
    candidateChannel = null
    routeCandidate = identityKey(selected)
    void load(true)
    const path = `/qa?candidate=${encodeURIComponent(id)}`
    if (`${router.path}${router.search}` !== path) router.go(path)
  }

  async function deploy() {
    const selected = selectedCandidate()
    if (!evidence || !evidenceIsLocal || !target || evidence.candidate.id !== selected.id) return
    const targetId = target
    const detail = detailRequest
    const action = ++actionRequest
    busy = 'deploy'
    error = null
    try {
      const deployment = await api.deployCandidate(selected.id, targetId)
      if (!actionStillCurrent(action, detail, selected)) return
      deploymentId = deployment.id
      await load()
    } catch (cause) {
      if (!actionStillCurrent(action, detail, selected)) return
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      if (actionStillCurrent(action, detail, selected)) busy = null
    }
  }

  async function verify() {
    const selected = selectedCandidate()
    if (!evidence || !evidenceIsLocal || !deploymentId || evidence.candidate.id !== selected.id) return
    const selectedDeployment = deploymentId
    const detail = detailRequest
    const action = ++actionRequest
    busy = 'verify'
    error = null
    try {
      await api.browserVerify(selectedDeployment, {
        start_path: startPath || '/',
        assertions: [
          { kind: 'url_path_is', path: expectedPath || startPath },
          { kind: 'screenshot', label: 'qa-result' },
        ],
      })
      if (!actionStillCurrent(action, detail, selected)) return
      await load()
    } catch (cause) {
      if (!actionStillCurrent(action, detail, selected)) return
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      if (actionStillCurrent(action, detail, selected)) busy = null
    }
  }

  async function prototype() {
    const selected = selectedCandidate()
    if (!evidence || !evidenceIsLocal || evidence.candidate.id !== selected.id) return
    const detail = detailRequest
    const action = ++actionRequest
    busy = 'prototype'
    error = null
    try {
      await api.buildPrototype(selected.id)
      if (!actionStillCurrent(action, detail, selected)) return
      await load()
    } catch (cause) {
      if (!actionStillCurrent(action, detail, selected)) return
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      if (actionStillCurrent(action, detail, selected)) busy = null
    }
  }

  function candidateLabel(item: EvidenceCandidate) {
    return item.work_items[0]?.title ?? item.reviews[0]?.title ?? `Candidate ${item.candidate.head_sha.slice(0, 12)}`
  }

  function clearFilters() {
    search = ''
    channel = ''
    runner = ''
    void loadCandidates(true)
  }

  function json(value: string) {
    try {
      return JSON.stringify(JSON.parse(value), null, 2)
    } catch {
      return value
    }
  }

  $effect(() => {
    const params = new URLSearchParams(router.search)
    const selected: CandidateIdentity = {
      id: params.get('candidate')?.trim() ?? '',
      owner: params.get('owner')?.trim() || null,
      channel: params.get('channel')?.trim() || null,
    }
    const key = identityKey(selected)
    if (!selected.id) {
      if (routeCandidate) {
        routeCandidate = ''
        clearDetail()
      }
      return
    }
    if (key === routeCandidate) return
    routeCandidate = key
    candidateId = selected.id
    candidateOwner = selected.owner
    candidateChannel = selected.channel
    void load(true)
  })
  onMount(() => {
    void loadCandidates(true)
  })
</script>

<div class="h4">Evidence <b>captured changes, checks, and browser results</b></div>
<p class="lede">Browse review evidence here, or choose a shared channel to inspect another node. QA actions stay on the owner.</p>

<section class="browse-panel">
  <div class="h5">Captured candidates</div>
  <form
    class="browse"
    onsubmit={(event) => {
      event.preventDefault()
      void loadCandidates(true)
    }}
  >
    <input bind:value={search} oninput={resetBrowseCursor} placeholder="Search task or review" aria-label="Search task or review evidence" />
    <select bind:value={channel} onchange={browseChannelChanged} aria-label="Evidence channel">
      <option value="">All locally held channels</option>
      {#each ordinaryChannels as item (item.name)}
        <option value={item.name}>{item.name}</option>
      {/each}
    </select>
    <select bind:value={runner} onchange={resetBrowseCursor} disabled={!channel} aria-label="Evidence runner">
      <option value="">{channel ? 'Serving node only' : 'Choose a shared channel first'}</option>
      {#each channelRunners as nodeId (nodeId)}
        <option value={nodeId}>{nodeLabel(store.nodes, nodeId)}{nodeId === store.node?.id ? ' (this node)' : ''}</option>
      {/each}
    </select>
    <button class="btn p" type="submit" disabled={browsing}>{browsing ? 'Searching…' : 'Search evidence'}</button>
  </form>

  {#if browsing && !browseLoaded}
    <div class="empty">Loading captured evidence…</div>
  {:else if browseError}
    <div class="banner crit">Evidence could not be loaded <b>· {browseError}</b></div>
  {:else if candidates.length === 0}
    {#if search.trim() || channel.trim() || runner.trim()}
      <div class="empty">No captured candidate evidence matches these filters. <button class="lnk" type="button" onclick={clearFilters}>Clear filters</button></div>
    {:else}
      <div class="empty">No candidate evidence has been captured on this node. Submit a committed change for review, or choose a shared channel and another runner. <a href="/work">Open Work</a></div>
    {/if}
  {:else}
    <div class="candidate-list">
      {#each candidates as item (item.candidate.id)}
        <article class:selected={evidence?.candidate.id === item.candidate.id}>
          <button class="candidate-choice" type="button" onclick={() => selectCandidate(item)}>
            <span><b>{candidateLabel(item)}</b><small>{item.candidate.channel} · {item.candidate.head_sha.slice(0, 12)} · captured {formatAge(item.candidate.captured_ms, clock.now)}{item.candidate.owner_node_id && item.candidate.owner_node_id !== store.node?.id ? ` · ${nodeLabel(store.nodes, item.candidate.owner_node_id)}` : ''}</small></span>
            <span class="open">Open evidence</span>
          </button>
          {#if item.work_items.length || item.reviews.length}
            <div class="relations">
              {#each item.work_items as work (work.id)}
                <a href="/work/{encodeURIComponent(work.id)}">Task · {work.title}</a>
              {/each}
              {#each item.reviews as review (review.id)}
                <a href="/reviews/{encodeURIComponent(review.id)}">Review · {review.title}</a>
              {/each}
              {#if item.more_work_items || item.more_reviews}
                <span class="relation-more">Additional recorded task or review links are omitted from this bounded card.</span>
              {/if}
            </div>
          {/if}
        </article>
      {/each}
    </div>
    {#if nextBefore}
      <button class="btn" type="button" onclick={() => loadCandidates(false)} disabled={browsing}>{browsing ? 'Loading…' : 'Load more evidence'}</button>
    {/if}
  {/if}
</section>

<details class="technical">
  <summary>Technical candidate lookup</summary>
  <p>Use a complete <code>commit:channel</code> only when you already have that immutable candidate identity on this node.</p>
  <div class="lookup">
    <input bind:value={candidateId} placeholder="Candidate id (commit:channel)" onkeydown={(event) => { if (event.key === 'Enter') selectTechnicalCandidate() }} />
    <button class="btn" type="button" onclick={selectTechnicalCandidate} disabled={busy !== null || !candidateId.trim()}>{busy === 'load' ? 'Loading…' : 'Load candidate'}</button>
  </div>
</details>

{#if error}<div class="banner crit">{error}</div>{/if}
{#if evidence}
  <section class="candidate">
    <div><b>Candidate {evidence.candidate.head_sha.slice(0, 12)}</b><span>{evidence.candidate.channel} · {evidenceOwner()}</span></div>
    <small>captured {formatAge(evidence.candidate.captured_ms, clock.now)} · {#if evidence.candidate.owner_session_id}<a href="/sessions/{evidence.candidate.owner_session_id}">Source session</a>{:else}Missing owner session; consequential QA is unavailable{/if}</small>
  </section>

  <section class="panel">
    <div class="h5">Configured targets</div>
    {#if evidence.targets.length === 0}
      <div class="empty">No QA target is configured on the evidence runner. A target must declare its deployment and digest-pinned browser images before it can be used.</div>
    {:else}
      <div class="targets">
        {#each evidence.targets as configured (configured.id)}
          <article class="target" class:blocked={configured.missing_grants.length > 0}>
            <b>{configured.id}</b>
            <code>{configured.origin}</code>
            <small>deployment {configured.execution_image}<br />browser {configured.browser_image}</small>
            <small>identity {configured.identity_header} at {configured.identity_url}</small>
            {#if configured.test_credential}<small>dedicated test account: {configured.test_credential}</small>{/if}
            {#if configured.missing_grants.length}
              <div class="missing">missing grants: {configured.missing_grants.join(', ')}</div>
            {:else}
              <div class="fresh">all required grants are active</div>
            {/if}
          </article>
        {/each}
      </div>
      {#if evidenceIsLocal}
        <div class="actions">
          <select bind:value={target}>{#each evidence.targets as configured (configured.id)}<option value={configured.id}>{configured.id}</option>{/each}</select>
          <button class="btn p" onclick={deploy} disabled={busy !== null || !target || evidence.candidate.id !== candidateId.trim()}>{busy === 'deploy' ? 'Deploying…' : 'Deploy candidate'}</button>
          <button class="btn" onclick={prototype} disabled={busy !== null || evidence.candidate.id !== candidateId.trim()}>{busy === 'prototype' ? 'Building…' : 'Build prototype'}</button>
        </div>
      {:else}
        <div class="notice">This is read-only evidence from runner {evidenceOwner()}. Deployment and prototype controls stay with that runner.</div>
      {/if}
    {/if}
  </section>

  <section class="panel">
    <div class="h5">Deployment identity</div>
    {#if evidence.deployments.length === 0}
      <div class="empty">No deployment observations for this candidate.</div>
    {:else}
      <div class="rows">
        {#each evidence.deployments as deployment (deployment.id)}
          <button class:chosen={deploymentId === deployment.id} class="row" type="button" onclick={() => (deploymentId = deployment.id)}>
            <span class:unknown={deployment.identity_state !== 'fresh'} class="dot"></span>
            <span><b>{deployment.target_id}</b> · {deployment.outcome}<small>{deployment.build_id} · identity {deployment.environment_identity ?? 'unobservable'} · {formatAge(deployment.observed_ms, clock.now)}</small></span>
            <code>{deployment.execution_image}</code>
          </button>
        {/each}
      </div>
    {/if}
  </section>

  <section class="panel">
    <div class="h5">Scoped browser verification</div>
    <p>Runs in the target’s isolated browser image. Every navigation, redirect, subresource, and WebSocket is constrained to its configured origins. A dedicated test account, if a scenario uses one, is separately granted and never appears here.</p>
    {#if evidenceIsLocal}
      <div class="form">
        <select bind:value={deploymentId} disabled={!evidence.deployments.length}>{#each evidence.deployments as deployment (deployment.id)}<option value={deployment.id}>{deployment.target_id} · {deployment.id}</option>{/each}</select>
        <input bind:value={startPath} placeholder="Start path, e.g. /login" />
        <input bind:value={expectedPath} placeholder="Expected final path" />
        <button class="btn p" onclick={verify} disabled={busy !== null || !deploymentId || evidence.candidate.id !== candidateId.trim()}>{busy === 'verify' ? 'Running browser…' : 'Run browser proof'}</button>
      </div>
    {:else}
      <div class="notice">Browser proof controls stay with runner {evidenceOwner()}; this view shows its recorded result.</div>
    {/if}
  </section>

  <section class="panel">
    <div class="h5">Browser runs and artifacts</div>
    {#if evidence.browser_runs.length === 0}
      <div class="empty">No browser verification has run.</div>
    {:else}
      <div class="runs">
        {#each evidence.browser_runs as run (run.id)}
          <article class:stale={run.evidence_state !== 'fresh'}>
            <header><b>{run.target_id}</b><span class="state">{run.outcome} · {run.evidence_state}</span></header>
            <small>deployment {run.deployment_id} · before {run.environment_before ?? 'unobservable'} · after {run.environment_after ?? 'unobservable'}</small>
            <details><summary>Assertions and bounded log</summary><pre>{json(run.assertions_json)}{run.log_tail ? `\n\n${run.log_tail}` : ''}</pre></details>
            <div class="artifacts">
              {#each evidence.assets.filter((asset) => asset.browser_run_id === run.id) as asset (asset.id)}
                {#if evidenceIsLocal}
                  <a href="/docs/{asset.channel}/{asset.slug}">{asset.kind} · {asset.slug}</a>
                {:else}
                  <span class="remote-artifact">{asset.kind} bundle is held by runner {evidenceOwner()}</span>
                {/if}
              {/each}
            </div>
          </article>
        {/each}
      </div>
    {/if}
  </section>

  <section class="panel">
    <div class="h5">Repository-derived prototypes</div>
    {#if evidence.prototypes.length === 0}
      <div class="empty">No prepared-runtime prototype build has been imported.</div>
    {:else}
      <div class="runs">
        {#each evidence.prototypes as prototype (prototype.id)}
          <article class:stale={prototype.outcome !== 'succeeded'}>
            <header><b>{prototype.outcome}</b><span>{prototype.source_revision}</span></header>
            <small>{prototype.build_image}</small>
            {#if prototype.document_id}
              {#if evidenceIsLocal}
                <a href="/docs/{prototype.channel}/{prototype.slug}">Open sandboxed preview bundle</a>
              {:else}
                <span class="remote-artifact">Preview bundle is held by runner {evidenceOwner()}</span>
              {/if}
            {/if}
            {#if prototype.detail}<details><summary>Build detail</summary><pre>{prototype.detail}</pre></details>{/if}
          </article>
        {/each}
      </div>
    {/if}
  </section>
{/if}

<style>
  .lookup, .actions, .form { display: flex; gap: .55rem; flex-wrap: wrap; align-items: center; }
  .lede { max-width: 66rem; margin: .45rem 0 0; color: var(--ink2); }
  .browse-panel { display: grid; gap: .7rem; margin-top: 1.3rem; }
  .browse { display: flex; gap: .55rem; flex-wrap: wrap; align-items: center; }
  .browse input:first-child { min-width: min(28rem, 100%); flex: 1; }
  .candidate-list { display: grid; gap: .45rem; }
  .candidate-list article { display: grid; gap: .45rem; background: var(--s1); border-left: 3px solid var(--acc); }
  .candidate-list article.selected { border-left-color: var(--ok); }
  .candidate-choice { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: .8rem; align-items: center; width: 100%; padding: .7rem .8rem; color: var(--ink); text-align: left; background: transparent; border: 0; cursor: pointer; }
  .candidate-choice > span:first-child { display: grid; gap: .18rem; min-width: 0; }
  .candidate-choice b, .candidate-choice small { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .candidate-choice small { display: block; }
  .open { color: var(--acc); font: 12px var(--mono); white-space: nowrap; }
  .relations { display: flex; flex-wrap: wrap; gap: .4rem .7rem; padding: 0 .8rem .7rem; }
  .relations a { overflow: hidden; max-width: 100%; color: var(--ink2); font: 12px var(--mono); text-overflow: ellipsis; white-space: nowrap; }
  .relation-more { color: var(--dim); font: 11px var(--mono); }
  .technical { display: grid; gap: .55rem; margin-top: 1.15rem; }
  .technical p { margin: 0; color: var(--ink2); font-size: .9rem; }
  .lookup input { min-width: min(36rem, 100%); flex: 1; }
  input, select { color: var(--ink); background: var(--s1); border: 1px solid var(--rule); border-radius: 4px; padding: .48rem .6rem; font: 12.5px var(--mono); min-width: 0; }
  .candidate { display: grid; gap: .2rem; padding: .8rem 1rem; background: var(--s1); border-left: 3px solid var(--acc); }
  .candidate div { display: flex; gap: .7rem; flex-wrap: wrap; } .candidate span, small { color: var(--ink2); font: 11.5px var(--mono); }
  .panel { margin-top: 1.3rem; display: grid; gap: .65rem; } .h5 { color: var(--dim); font: 12px var(--mono); text-transform: uppercase; letter-spacing: .08em; }
  .panel p { margin: 0; color: var(--ink2); max-width: 68rem; }
  .targets, .runs { display: grid; gap: .5rem; grid-template-columns: repeat(auto-fit, minmax(250px, 1fr)); }
  .target, .runs article { display: grid; gap: .35rem; background: var(--s1); padding: .75rem .85rem; border-left: 3px solid var(--ok); min-width: 0; }
  .target.blocked, .runs article.stale { border-left-color: var(--wait); } .target code, .row code { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--ink2); }
  .missing { color: var(--wait); font: 12px var(--mono); } .fresh { color: var(--ok); font: 12px var(--mono); }
  .rows { display: grid; gap: .35rem; } .row { display: grid; grid-template-columns: auto minmax(0, 1fr) minmax(10rem, .9fr); gap: .6rem; align-items: center; text-align: left; color: var(--ink); background: var(--s1); border: 0; border-left: 3px solid var(--ok); padding: .65rem; cursor: pointer; } .row.chosen { background: var(--s2); } .row small { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .dot { width: .55rem; height: .55rem; border-radius: 50%; background: var(--ok); } .dot.unknown { background: var(--wait); }
  .runs header { display: flex; justify-content: space-between; gap: .5rem; } .state { color: var(--ink2); font: 12px var(--mono); } details { min-width: 0; } summary { cursor: pointer; color: var(--acc); font: 12px var(--mono); } pre { max-height: 18rem; overflow: auto; white-space: pre-wrap; color: var(--ink2); font: 11px/1.45 var(--mono); } .artifacts { display: flex; flex-wrap: wrap; gap: .6rem; } .artifacts a, .runs a { font: 12px var(--mono); } .notice, .remote-artifact { color: var(--wait); font: 12px var(--mono); }
  @media (max-width: 650px) {
    .row { grid-template-columns: auto minmax(0, 1fr); }
    .row code { grid-column: 2; }
    .form > *, .browse > * { width: 100%; }
    .candidate-choice, .relations a { display: flex; align-items: center; min-height: 44px; }
  }
</style>
