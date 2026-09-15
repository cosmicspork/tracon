<script lang="ts">
  import { onMount } from 'svelte'
  import { ApiError } from '../../lib/api'
  import { policyAdmin, type PolicyRollout, type PolicyStatus, type SignedPolicyBundle } from '../../lib/policy-admin'
  import { store } from '../../lib/store.svelte'
  import Card from './Card.svelte'

  let { onapplied }: { onapplied?: () => Promise<void> } = $props()

  let status = $state<PolicyStatus | null>(null)
  let draft = $state('')
  let preview = $state<SignedPolicyBundle | null>(null)
  let previewBaseline = $state<string | null>(null)
  let selected = $state<string[]>([])
  let loading = $state(true)
  let busy = $state(false)
  let retrying = $state<string | null>(null)
  let notice = $state('')
  let error = $state('')
  let setupConfirmed = $state(false)

  const peers = $derived(store.nodes.filter((node) => !node.is_self))
  const previewCurrent = $derived(
    Boolean(preview && status && preview.toml === draft && previewBaseline === installedHash(status)),
  )
  const local = $derived(store.node?.loopback === true)
  const canSign = $derived(local && status?.signing_key_present === true)

  function installedHash(value: PolicyStatus | null) {
    return value?.installed?.bundle_sha256 ?? null
  }

  function discardPreview() {
    preview = null
    previewBaseline = null
  }

  async function refresh(resetDraft = false): Promise<boolean> {
    loading = true
    error = ''
    try {
      const next = await policyAdmin.status()
      const previewInvalidated = preview
        && (preview.toml !== draft || previewBaseline !== installedHash(next))
      status = next
      if (resetDraft || !draft) draft = next.installed?.toml ?? ''
      if (previewInvalidated) {
        discardPreview()
        notice = 'The installed policy changed after this preview. Create a fresh preview before applying.'
      }
      return true
    } catch (e) {
      status = null
      discardPreview()
      error = message(e)
      return false
    } finally {
      loading = false
    }
  }

  async function refreshAfterLocalMutation(success: string) {
    // A known successful local mutation makes the preceding status stale. Do
    // not continue presenting it as current if the follow-up read fails.
    status = null
    const refreshed = await refresh(false)
    const related = await Promise.allSettled([
      store.refetch(),
      onapplied?.() ?? Promise.resolve(),
    ])
    const relatedErrors = related
      .filter((result): result is PromiseRejectedResult => result.status === 'rejected')
      .map((result) => message(result.reason))
    notice = refreshed
      ? success
      : `${success} The policy-status refresh failed, so installed and running state are not displayed.`
    if (relatedErrors.length) {
      error = [error, `Related views could not be refreshed: ${relatedErrors.join('; ')}`]
        .filter(Boolean)
        .join(' ')
    }
  }

  async function makePreview() {
    if (!status) return
    busy = true
    notice = ''
    error = ''
    const baseline = installedHash(status)
    try {
      preview = await policyAdmin.preview(draft)
      previewBaseline = baseline
      notice = `Preview signed: ${short(preview.bundle_sha256)}. Nothing has been applied.`
    } catch (e) {
      discardPreview()
      error = message(e)
    } finally {
      busy = false
    }
  }

  async function apply() {
    if (!preview || !previewCurrent || !status) return
    const candidate = preview
    const baseline = previewBaseline
    busy = true
    notice = ''
    error = ''
    try {
      const result = await policyAdmin.apply(candidate, baseline, selected)
      draft = result.installed.toml
      discardPreview()
      await refreshAfterLocalMutation(
        result.rollout
          ? `Applied locally. ${result.rollout.targets.length} node${result.rollout.targets.length === 1 ? '' : 's'} selected; delivery is not installation.`
          : 'Applied locally.',
      )
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        discardPreview()
        const refreshed = await refresh(false)
        error = `${message(e)} Create a fresh preview before applying.${refreshed ? '' : ' Current status could not be refreshed.'}`
      } else {
        error = message(e)
      }
    } finally {
      busy = false
    }
  }

  async function initialize() {
    busy = true
    notice = ''
    error = ''
    try {
      const initial = await policyAdmin.initialize()
      draft = initial.toml
      discardPreview()
      await refreshAfterLocalMutation(`Created the initial signed policy: ${short(initial.bundle_sha256)}.`)
    } catch (e) {
      error = message(e)
    } finally {
      busy = false
    }
  }

  async function retry(rollout: PolicyRollout) {
    retrying = rollout.id
    notice = ''
    error = ''
    try {
      await policyAdmin.retry(rollout.id)
      const refreshed = await refresh()
      notice = refreshed
        ? `Retried ${short(rollout.bundle_sha256)}. Sent is still not applied until a receiver confirms it.`
        : `Retried ${short(rollout.bundle_sha256)}. The status refresh failed; delivery is still not installation.`
    } catch (e) {
      error = message(e)
    } finally {
      retrying = null
    }
  }

  function toggle(nodeId: string) {
    selected = selected.includes(nodeId) ? selected.filter((id) => id !== nodeId) : [...selected, nodeId]
  }

  function short(value: string | null | undefined) {
    return value ? `${value.slice(0, 12)}…` : 'none'
  }

  function when(ms: number | null) {
    return ms ? new Date(ms).toLocaleString() : '—'
  }

  function message(error: unknown) {
    return error instanceof Error ? error.message : String(error)
  }

  onMount(() => {
    void refresh(true)
  })
</script>

<Card title="Signed policy" note="What this node has installed and is actually running. Edits apply only the exact signed preview you choose.">
  {#snippet actions()}
    <button class="lnk" onclick={() => refresh(false)} disabled={loading || busy}>Refresh</button>
  {/snippet}

  {#if loading}
    <p class="dim">Reading installed files and the running policy…</p>
  {:else if status}
    <section class="policy-summary" aria-label="Installed and running policy">
      <div>
        <p class="eyebrow">Verified installed files</p>
        {#if status.installed}
          <h3>Version {status.installed.policy.version} · {status.installed.policy.rules.length} rules</h3>
          <p>Verified bundle {short(status.installed.bundle_sha256)} is installed on disk. The running process policy is shown separately.</p>
        {:else}
          <h3>No verified signed policy installed</h3>
          <p>New setups start with the shipped rules only after an explicit local confirmation.</p>
        {/if}
      </div>
      <div>
        <p class="eyebrow">Running process policy</p>
        <h3>Version {status.running.version} · {status.running.rule_count} rules</h3>
        {#if status.running.trusted}
          <p>Trusted policy currently used for decisions by this node.</p>
        {:else}
          <p>No verified policy is running; unmatched actions ask.</p>
        {/if}
      </div>
      <div class="identity" aria-label="Policy trust identity">
        <span>Installed trust identity</span>
        <code>{status.installed?.trust_identity ?? status.trust_identity ?? 'Not configured'}</code>
        {#if status.installation}
          <small>Recorded source {status.installation.source_node ? short(status.installation.source_node) : 'not recorded'} · applied {when(status.installation.applied_ms)}</small>
        {:else}
          <small>No matching installed-bundle record.</small>
        {/if}
      </div>
    </section>

    {#if status.installed_error}
      <p class="bad" role="alert">The installed signed files cannot be verified: {status.installed_error}. They will not be overwritten by this panel.</p>
    {:else if !status.installed}
      <details class="initialize">
        <summary>Initialize the shipped policy</summary>
        <div class="details-body">
        <p>This creates this node’s first signing identity and installs the shipped policy. Existing keys, signatures, and bundles are never replaced.</p>
        <label class="confirm"><input type="checkbox" bind:checked={setupConfirmed} /> I understand this creates a new local signing identity.</label>
        <button class="btn primary" onclick={initialize} disabled={busy || !setupConfirmed || !local}>{busy ? 'Initializing…' : 'Create initial signed policy'}</button>
        {#if !local}<p class="helper">Open this node locally to initialize its policy.</p>{/if}
        </div>
      </details>
    {:else}
      <details class="edit-policy">
        <summary>Edit policy</summary>
        <div class="details-body">
        <p class="helper">{!local ? 'Open the signing node locally to edit its policy.' : !status.signing_key_present ? 'This node has no signing key. Edit on the node that signed this policy, then roll it out here.' : 'Preview with the existing signing key, then apply the exact signed bytes. The key never leaves this node.'}</p>
        <label class="policy-editor">
          <span>Policy TOML</span>
          <textarea bind:value={draft} oninput={discardPreview} spellcheck="false" disabled={busy}></textarea>
        </label>
        <div class="actions">
          <button class="btn" onclick={makePreview} disabled={busy || !draft.trim() || !canSign}>{busy ? 'Working…' : 'Preview & sign'}</button>
          {#if preview && previewCurrent}
            <span class="chip">Preview {short(preview.bundle_sha256)} · baseline {short(previewBaseline)} · v{preview.policy.version} · {preview.policy.rules.length} rules</span>
          {/if}
        </div>

        <fieldset disabled={busy}>
          <legend>Mesh rollout targets</legend>
          <p class="helper">Only selected nodes receive this exact preview. Delivery is encrypted; a node is never shown as applied until it returns an authenticated receipt.</p>
          {#if peers.length}
            <div class="targets">
              {#each peers as node (node.id)}
                <label class="target">
                  <input type="checkbox" checked={selected.includes(node.id)} onchange={() => toggle(node.id)} />
                  <span><strong>{node.name || short(node.id)}</strong><small>{node.reachable ? 'Reachable' : 'Offline'} · {node.harness.id} {node.harness.found ?? 'unknown'}</small></span>
                </label>
              {/each}
            </div>
          {:else}
            <p class="dim">No peer nodes are available. Applying affects this managing node only.</p>
          {/if}
        </fieldset>

        <div class="apply-row">
          <button class="btn primary" onclick={apply} disabled={busy || !preview || !previewCurrent}>
            Apply exact preview{selected.length ? ` & roll out to ${selected.length}` : ''}
          </button>
          <small>Applying replaces the local bundle only if verified installed files still match the baseline captured for this preview. A changed baseline requires a fresh preview. It never rotates a key.</small>
        </div>
        </div>
      </details>
    {/if}
  {/if}

  {#if notice}<p class="ok" role="status">{notice}</p>{/if}
  {#if error}<p class="bad" role="alert">{error}</p>{/if}
</Card>

{#if status?.rollouts.length}
<Card title="Rollout history" note="Durable receipts from the nodes each policy was sent to.">
  <div class="rollouts">
    {#each status.rollouts as rollout (rollout.id)}
      <article>
        <header>
          <div>
            <strong>Policy {short(rollout.bundle_sha256)}</strong>
            <small>v{rollout.policy_version} · created {when(rollout.created_ms)}</small>
          </div>
          <button class="lnk" onclick={() => retry(rollout)} disabled={retrying === rollout.id}>
            {retrying === rollout.id ? 'Retrying…' : 'Retry unconfirmed'}
          </button>
        </header>
        <ul>
          {#each rollout.targets as target (target.node_id)}
            {@const node = peers.find((candidate) => candidate.id === target.node_id)}
            <li class:applied={target.confirmation === 'applied'} class:rejected={target.confirmation === 'rejected'}>
              <div>
                <strong>{node?.name || short(target.node_id)}</strong>
                <small>{short(target.node_id)} · attempts {target.attempts} · last sent {when(target.last_sent_ms)}</small>
              </div>
              <div class="receipt">
                <span>{target.confirmation === 'applied' ? 'Applied' : target.confirmation === 'rejected' ? 'Rejected' : target.status === 'offline' ? 'Offline / unconfirmed' : 'Sent / unconfirmed'}</span>
                <small>{target.compatibility === 'receipt_capable' ? 'Receipt-capable peer' : 'Legacy or unknown peer — no authenticated receipt yet.'}</small>
                {#if target.detail}<small>{target.detail}</small>{/if}
              </div>
            </li>
          {/each}
        </ul>
      </article>
    {/each}
  </div>
</Card>
{/if}

<style>
  h3, p { margin: 0; }
  h3 { font: 600 14px var(--sans); }
  .eyebrow { color: var(--ink2); font: 500 11px/1.3 var(--mono); text-transform: uppercase; letter-spacing: .08em; }
  .helper, small { color: var(--ink2); line-height: 1.45; font-size: 12.5px; }
  .policy-summary, fieldset, .rollouts article, details { border: 0; border-radius: 4px; background: var(--s2); padding: 12px 14px; }
  .policy-summary { display: grid; grid-template-columns: repeat(auto-fit, minmax(16rem, 1fr)); gap: 14px; }
  .policy-summary > div { display: grid; gap: .3rem; align-content: start; }
  .identity { min-width: 0; }
  .identity > span { color: var(--ink2); font: 500 11px/1.3 var(--mono); text-transform: uppercase; letter-spacing: .08em; }
  .identity code { overflow-wrap: anywhere; font-size: .8rem; }
  .details-body { display: grid; gap: 12px; margin-top: 12px; }
  .details-body > .btn { justify-self: start; }
  summary { color: var(--acc); cursor: pointer; font-weight: 600; }
  .confirm { display: flex; align-items: center; gap: .6rem; min-height: 36px; color: var(--ink); }
  .policy-editor { display: grid; gap: .5rem; }
  .policy-editor > span { color: var(--ink2); font: 500 11px/1.3 var(--mono); text-transform: uppercase; letter-spacing: .08em; }
  textarea { min-height: 16rem; width: 100%; resize: vertical; border: 0; border-radius: 4px; background: var(--s1); color: var(--ink); padding: .75rem; font: .82rem/1.45 var(--mono); }
  fieldset { display: grid; gap: .75rem; background: none; padding: 0; margin: 0; }
  legend { color: var(--ink); font-weight: 600; padding: 0 0 .4rem; }
  .targets { display: grid; gap: .5rem; grid-template-columns: repeat(auto-fit, minmax(15rem, 1fr)); }
  .target { display: flex; align-items: center; gap: .7rem; min-height: 44px; padding: .45rem .6rem; border-radius: 4px; background: var(--s1); }
  .target span { display: grid; }
  .actions, .apply-row { display: flex; flex-wrap: wrap; align-items: center; gap: 8px 12px; }
  .apply-row small { flex: 1 1 20rem; }
  .btn.primary { background: var(--acc); color: var(--acc-ink); }
  .chip { font: 12px var(--mono); color: var(--ink2); }
  .chip::before { display: none; }
  .ok { color: var(--ok); }
  .bad { color: var(--crit); }
  .dim { color: var(--ink2); }
  .rollouts { display: grid; gap: .75rem; }
  .rollouts article { display: grid; gap: .75rem; }
  .rollouts article > header { display: flex; flex-wrap: wrap; align-items: start; justify-content: space-between; gap: 8px 16px; }
  .rollouts article > header > div { display: grid; }
  .rollouts ul { display: grid; gap: .5rem; list-style: none; margin: 0; padding: 0; }
  .rollouts li { display: flex; justify-content: space-between; gap: 1rem; border-left: 3px solid var(--ink2); padding: .4rem .6rem; }
  .rollouts li.applied { border-color: var(--ok); }
  .rollouts li.rejected { border-color: var(--crit); }
  .rollouts li div { display: grid; }
  .receipt { text-align: right; max-width: 30rem; }
  @media (max-width: 42rem) { .rollouts li { align-items: stretch; flex-direction: column; } .receipt { text-align: left; } }
</style>
