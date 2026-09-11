<script lang="ts">
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import type { QaEvidence } from '../lib/types'

  let candidateId = $state('')
  let evidence = $state<QaEvidence | null>(null)
  let error = $state<string | null>(null)
  let busy = $state<string | null>(null)
  let target = $state('')
  let deploymentId = $state('')
  let startPath = $state('/')
  let expectedPath = $state('/')

  async function load() {
    const id = candidateId.trim()
    if (!id) return
    busy = 'load'
    error = null
    try {
      evidence = await api.qaEvidence(id)
      if (!target && evidence.targets.length) target = evidence.targets[0].id
      if (!deploymentId && evidence.deployments.length) deploymentId = evidence.deployments[0].id
    } catch (cause) {
      evidence = null
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = null
    }
  }

  async function deploy() {
    if (!evidence || !target) return
    busy = 'deploy'
    error = null
    try {
      const deployment = await api.deployCandidate(evidence.candidate.id, target)
      deploymentId = deployment.id
      await load()
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = null
    }
  }

  async function verify() {
    if (!deploymentId) return
    busy = 'verify'
    error = null
    try {
      await api.browserVerify(deploymentId, {
        start_path: startPath || '/',
        assertions: [
          { kind: 'url_path_is', path: expectedPath || startPath },
          { kind: 'screenshot', label: 'qa-result' },
        ],
      })
      await load()
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = null
    }
  }

  async function prototype() {
    if (!evidence) return
    busy = 'prototype'
    error = null
    try {
      await api.buildPrototype(evidence.candidate.id)
      await load()
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = null
    }
  }

  function json(value: string) {
    try {
      return JSON.stringify(JSON.parse(value), null, 2)
    } catch {
      return value
    }
  }
</script>

<div class="h4">QA evidence <b>candidate-bound deployment and browser proof</b></div>
<div class="lookup">
  <input bind:value={candidateId} placeholder="Candidate id (commit:channel)" onkeydown={(event) => event.key === 'Enter' && load()} />
  <button class="btn p" onclick={load} disabled={busy !== null || !candidateId.trim()}>{busy === 'load' ? 'Loading…' : 'Load candidate'}</button>
</div>

{#if error}<div class="banner crit">{error}</div>{/if}

{#if evidence}
  <section class="candidate">
    <div><b>{evidence.candidate.id}</b><span>{evidence.candidate.channel} · {evidence.candidate.head_sha}</span></div>
    <small>captured {formatAge(evidence.candidate.captured_ms, clock.now)} · owner session {evidence.candidate.owner_session_id || 'missing — cannot authorize consequential QA'}</small>
  </section>

  <section class="panel">
    <div class="h5">Configured targets</div>
    {#if evidence.targets.length === 0}
      <div class="empty">No QA target is configured on this node. A target must declare its deployment and digest-pinned browser images before it can be used.</div>
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
      <div class="actions">
        <select bind:value={target}>{#each evidence.targets as configured (configured.id)}<option value={configured.id}>{configured.id}</option>{/each}</select>
        <button class="btn p" onclick={deploy} disabled={busy !== null || !target}>{busy === 'deploy' ? 'Deploying…' : 'Deploy candidate'}</button>
        <button class="btn" onclick={prototype} disabled={busy !== null}>{busy === 'prototype' ? 'Building…' : 'Build prototype'}</button>
      </div>
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
    <div class="form">
      <select bind:value={deploymentId} disabled={!evidence.deployments.length}>{#each evidence.deployments as deployment (deployment.id)}<option value={deployment.id}>{deployment.target_id} · {deployment.id}</option>{/each}</select>
      <input bind:value={startPath} placeholder="Start path, e.g. /login" />
      <input bind:value={expectedPath} placeholder="Expected final path" />
      <button class="btn p" onclick={verify} disabled={busy !== null || !deploymentId}>{busy === 'verify' ? 'Running browser…' : 'Run browser proof'}</button>
    </div>
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
                <a href="/docs/{asset.channel}/{asset.slug}">{asset.kind} · {asset.slug}</a>
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
            {#if prototype.document_id}<a href="/docs/{prototype.channel}/{prototype.slug}">Open sandboxed preview bundle</a>{/if}
            {#if prototype.detail}<details><summary>Build detail</summary><pre>{prototype.detail}</pre></details>{/if}
          </article>
        {/each}
      </div>
    {/if}
  </section>
{/if}

<style>
  .lookup, .actions, .form { display: flex; gap: .55rem; flex-wrap: wrap; align-items: center; }
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
  .runs header { display: flex; justify-content: space-between; gap: .5rem; } .state { color: var(--ink2); font: 12px var(--mono); } details { min-width: 0; } summary { cursor: pointer; color: var(--acc); font: 12px var(--mono); } pre { max-height: 18rem; overflow: auto; white-space: pre-wrap; color: var(--ink2); font: 11px/1.45 var(--mono); } .artifacts { display: flex; flex-wrap: wrap; gap: .6rem; } .artifacts a, .runs a { font: 12px var(--mono); }
  @media (max-width: 650px) { .row { grid-template-columns: auto minmax(0, 1fr); } .row code { grid-column: 2; } .form > * { width: 100%; } }
</style>
