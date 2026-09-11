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
      notice = `Staged ${staged.candidate_id}. Review it below, then explicitly import it.`
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
      notice = `Started continuation session ${imported.session.id.slice(0, 12)} from immutable candidate ${transfer.candidate_id}.`
      await refresh()
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      busy = null
    }
  }
</script>

<details class="inbox">
  <summary>Continuity transfer inbox</summary>
  <p>
    A package is only staged here. Import creates a new isolated workspace and session; it never changes the source
    session, transfers credentials, or adopts a live harness.
  </p>
  <div class="actions">
    <label class="upload">
      Stage signed package
      <input type="file" accept="application/json" onchange={upload} disabled={busy !== null} />
    </label>
    <button class="btn ghost" onclick={refresh} disabled={loading || busy !== null}>Refresh</button>
  </div>
  <label>
    Model for imported session <small>empty uses the channel phase binding</small>
    <input bind:value={model} placeholder="channel default" autocomplete="off" />
  </label>
  {#if loading}
    <p>Reading staged packages…</p>
  {:else if transfers.length === 0}
    <p>No staged packages on this node.</p>
  {:else}
    <ul>
      {#each transfers as transfer (transfer.id)}
        <li>
          <div>
            <strong>{transfer.candidate_id}</strong>
            <span>
              {transfer.channel} · from {transfer.origin_node.slice(0, 12)} ·
              {new Date(transfer.created_ms).toLocaleString()} · {transfer.files} files ·
              {transfer.documents} docs · {transfer.memories} memories
              {#if transfer.note} · note: {transfer.note}{/if}
              {#if transfer.import_state === 'imported'} · imported {transfer.session_id?.slice(0, 12)}{/if}
              {#if transfer.import_state === 'preparing'} · import outcome unknown{/if}
              {#if transfer.import_state === 'failed'} · previous import failed; retry is explicit{/if}
            </span>
          </div>
          <button
            class="btn"
            onclick={() => importTransfer(transfer)}
            disabled={busy !== null || (transfer.import_state !== null && transfer.import_state !== 'failed')}
          >
            {busy === transfer.id
              ? 'Preparing workspace…'
              : transfer.import_state === 'imported'
                ? 'Imported'
                : transfer.import_state === 'preparing'
                  ? 'Outcome unknown'
                  : transfer.import_state === 'failed'
                    ? 'Retry import'
                    : 'Confirm import'}
          </button>
        </li>
      {/each}
    </ul>
  {/if}
  {#if notice}<p class="ok">{notice}</p>{/if}
  {#if error}<p class="bad">{error}</p>{/if}
</details>

<style>
  .inbox { margin: 14px 0; padding: 9px 11px; background: var(--s1); border-left: 3px solid var(--ink2); }
  summary { cursor: pointer; font: 600 13px var(--sans); }
  p, label, span { font: 12px var(--mono); color: var(--ink2); }
  label { display: block; margin-top: 8px; }
  small { color: var(--dim); }
  input { width: 100%; box-sizing: border-box; margin-top: 3px; }
  .upload input { width: auto; margin-left: 7px; }
  .actions { display: flex; align-items: end; gap: 8px; }
  ul { list-style: none; margin: 10px 0 0; padding: 0; }
  li { display: flex; justify-content: space-between; gap: 10px; align-items: center; padding: 8px 0; border-top: 1px solid var(--line); }
  strong, span { display: block; }
  .ghost { background: transparent; color: var(--ink2); }
  .ok { color: var(--ok); }
  .bad { color: var(--crit); }
</style>
