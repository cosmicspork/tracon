<script lang="ts">
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import type { CandidateTransfer, TransferInboxItem } from '../lib/types'

  let transfers = $state<TransferInboxItem[]>([])
  let loading = $state(true)
  let busy = $state<string | null>(null)
  let model = $state('')
  let notice = $state<string | null>(null)
  let error = $state<string | null>(null)

  async function refresh() {
    loading = true
    try {
      transfers = (await api.transfers()).transfers
      error = null
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      loading = false
    }
  }

  onMount(refresh)

  async function upload(event: Event) {
    const input = event.currentTarget as HTMLInputElement
    const file = input.files?.[0]
    input.value = ''
    if (!file || busy) return
    busy = 'upload'
    notice = null
    error = null
    try {
      const transfer = JSON.parse(await file.text()) as CandidateTransfer
      const staged = await api.stageTransfer(transfer)
      notice = `Staged ${staged.candidate_id}. Review it below, then explicitly start an isolated continuation session.`
      await refresh()
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      busy = null
    }
  }

  async function importTransfer(transfer: TransferInboxItem) {
    if (busy) return
    busy = transfer.id
    notice = null
    error = null
    try {
      const body = model ? { confirm: true as const, model } : { confirm: true as const }
      const imported = await api.importTransfer(transfer.id, body)
      notice = `Started isolated continuation session ${imported.session.id.slice(0, 12)} from immutable candidate ${transfer.candidate_id}.`
      await refresh()
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      busy = null
    }
  }
</script>

<details class="inbox">
  <summary>Import a session</summary>
  <p class="consequence">
    Choose a signed package to stage it here. Starting it creates a new isolated workspace and session; the source
    session remains unchanged, and neither credentials nor a live harness are transferred.
  </p>
  <div class="actions">
    <label class="upload">
      <span>Signed transfer package</span>
      <input type="file" accept="application/json" onchange={upload} disabled={busy !== null} />
    </label>
    <button class="btn ghost" type="button" onclick={refresh} disabled={loading || busy !== null}>Refresh packages</button>
  </div>
  <label class="model">
    <span>Continuation model</span>
    <small>Leave empty to use the channel phase binding.</small>
    <input bind:value={model} placeholder="Channel default" autocomplete="off" />
  </label>
  {#if loading}
    <p>Reading staged packages…</p>
  {:else if transfers.length === 0}
    <div class="empty">No staged packages on this node. Choose a signed package from the source node or a portable download to inspect and start it here.</div>
  {:else}
    <ul>
      {#each transfers as transfer (transfer.id)}
        <li>
          <div class="transfer-meta">
            <strong>{transfer.candidate_id}</strong>
            <span>
              {transfer.channel} · source node {transfer.origin_node.slice(0, 12)} ·
              {new Date(transfer.created_ms).toLocaleString()} · {transfer.files} files ·
              {transfer.documents} docs · {transfer.memories} memories
              {#if transfer.note} · note: {transfer.note}{/if}
              {#if transfer.import_state === 'imported'} · continuation {transfer.session_id?.slice(0, 12)} started{/if}
              {#if transfer.import_state === 'preparing'} · import outcome unknown{/if}
              {#if transfer.import_state === 'failed'} · previous import failed; retry is explicit{/if}
            </span>
          </div>
          <button
            class="btn"
            type="button"
            onclick={() => importTransfer(transfer)}
            disabled={busy !== null || (transfer.import_state !== null && transfer.import_state !== 'failed')}
          >
            {busy === transfer.id
              ? 'Preparing isolated workspace…'
              : transfer.import_state === 'imported'
                ? 'Session started'
                : transfer.import_state === 'preparing'
                  ? 'Import outcome unknown'
                  : transfer.import_state === 'failed'
                    ? 'Retry isolated session'
                    : 'Start isolated session'}
          </button>
        </li>
      {/each}
    </ul>
  {/if}
  {#if notice}<p class="ok">{notice}</p>{/if}
  {#if error}<p class="bad">{error}</p>{/if}
</details>

<style>
  summary { cursor: pointer; color: var(--acc); font: 500 13px var(--sans); }
  .consequence, .transfer-meta, .model, .upload, .inbox > p {
    font: 12px var(--mono);
    color: var(--ink2);
  }
  .consequence { margin: 4px 0 10px; }
  .actions {
    display: flex;
    align-items: end;
    flex-wrap: wrap;
    gap: 8px;
  }
  .upload, .model {
    display: grid;
    gap: 3px;
  }
  .upload span, .model span {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  .model {
    margin-top: 10px;
  }
  small { color: var(--dim); }
  .model input { width: 100%; }
  ul { list-style: none; margin: 10px 0 0; padding: 0; }
  li {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    align-items: center;
    padding: 10px 0;
    border-top: 1px solid var(--rule);
  }
  .transfer-meta { min-width: 0; }
  strong, .transfer-meta span { display: block; }
  strong { color: var(--ink); font: 600 12px var(--mono); }
  .ghost { background: transparent; color: var(--ink2); }
  .ok { color: var(--ok); }
  .bad { color: var(--crit); }
  @media (max-width: 700px) {
    li {
      align-items: stretch;
      flex-direction: column;
    }
    li .btn {
      width: 100%;
    }
  }
</style>
