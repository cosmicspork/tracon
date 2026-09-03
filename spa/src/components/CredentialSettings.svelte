<script lang="ts">
  import { api } from '../lib/api'
  import { store } from '../lib/store.svelte'
  import type { CredentialSummary } from '../lib/types'

  type ForgeName = 'github' | 'gitlab'
  const forges: { forge: ForgeName; name: 'gh' | 'glab'; label: string }[] = [
    { forge: 'github', name: 'gh', label: 'GitHub' },
    { forge: 'gitlab', name: 'glab', label: 'GitLab' },
  ]

  let credentials = $state<CredentialSummary[]>([])
  let loading = $state(true)
  let editing = $state<Record<ForgeName, boolean>>({ github: false, gitlab: false })
  let token = $state<Record<ForgeName, string>>({ github: '', gitlab: '' })
  let selected = $state<Record<ForgeName, string[]>>({ github: [], gitlab: [] })
  let busy = $state<ForgeName | null>(null)
  let errors = $state<Record<ForgeName, string>>({ github: '', gitlab: '' })
  let notices = $state<Record<ForgeName, string>>({ github: '', gitlab: '' })
  let confirming = $state<ForgeName | null>(null)
  let toml = $state('')
  let importing = $state(false)
  let importError = $state('')
  let imported = $state<string[]>([])

  async function refetch() {
    try {
      credentials = (await api.credentials()).credentials
    } finally {
      loading = false
    }
  }
  void refetch()

  function current(name: string): CredentialSummary | undefined {
    return credentials.find((credential) => credential.name === name)
  }

  function pinnedToPeers(summary: CredentialSummary | undefined): boolean {
    return Boolean(
      summary?.nodes.some((node) => node !== store.node?.id),
    )
  }

  function edit(forge: ForgeName, summary: CredentialSummary | undefined) {
    token = { ...token, [forge]: '' }
    selected = {
      ...selected,
      [forge]: summary?.channels.length
        ? [...summary.channels]
        : store.channels.filter((channel) => !channel.archived).map((channel) => channel.name),
    }
    errors = { ...errors, [forge]: '' }
    notices = { ...notices, [forge]: '' }
    editing = { ...editing, [forge]: true }
    confirming = null
  }

  function cancelEdit(forge: ForgeName) {
    token = { ...token, [forge]: '' }
    editing = { ...editing, [forge]: false }
    errors = { ...errors, [forge]: '' }
  }

  function toggleChannel(forge: ForgeName, channel: string, checked: boolean) {
    const channels = selected[forge].filter((name) => name !== channel)
    if (checked) channels.push(channel)
    selected = { ...selected, [forge]: channels }
  }

  async function save(forge: ForgeName) {
    const value = token[forge].trim()
    if (!value || busy) return
    busy = forge
    errors = { ...errors, [forge]: '' }
    notices = { ...notices, [forge]: '' }
    try {
      await api.putForgeCredential(forge, value, selected[forge])
      token = { ...token, [forge]: '' }
      selected = { ...selected, [forge]: [] }
      editing = { ...editing, [forge]: false }
      notices = { ...notices, [forge]: 'Token saved.' }
      await refetch()
    } catch (caught) {
      errors = { ...errors, [forge]: caught instanceof Error ? caught.message : String(caught) }
    } finally {
      busy = null
    }
  }

  async function remove(forge: ForgeName) {
    if (busy) return
    busy = forge
    errors = { ...errors, [forge]: '' }
    notices = { ...notices, [forge]: '' }
    try {
      await api.deleteForgeCredential(forge)
      confirming = null
      token = { ...token, [forge]: '' }
      selected = { ...selected, [forge]: [] }
      editing = { ...editing, [forge]: false }
      notices = { ...notices, [forge]: 'Token removed from this node.' }
      await refetch()
    } catch (caught) {
      errors = { ...errors, [forge]: caught instanceof Error ? caught.message : String(caught) }
    } finally {
      busy = null
    }
  }

  async function importCredential() {
    if (!toml.trim() || importing) return
    importing = true
    importError = ''
    imported = []
    try {
      imported = (await api.importCredentials(toml)).imported
      toml = ''
      await refetch()
    } catch (caught) {
      importError = caught instanceof Error ? caught.message : String(caught)
    } finally {
      importing = false
    }
  }
</script>

<div class="forge-list">
  {#each forges as forge (forge.forge)}
    {@const summary = current(forge.name)}
    {@const peerCopies = pinnedToPeers(summary)}
    <div class="forge-row">
      <span class="bar" class:saved={Boolean(summary)}></span>
      <div class="forge-name">
        <strong>{forge.label}</strong>
        <small>{forge.name}</small>
      </div>
      <div class="forge-state">
        {#if loading}
          <span class="dim">Checking…</span>
        {:else if summary}
          <span><span class="chip">Token saved</span> · {summary.channels.join(', ') || 'no channels'}</span>
          <small>
            {summary.nodes.length === 0
              ? 'This node only'
              : `${summary.nodes.length} node${summary.nodes.length === 1 ? '' : 's'} bound`}
          </small>
        {:else}
          <span class="dim">No token saved.</span>
        {/if}

        {#if editing[forge.forge]}
          <div class="editor">
            <label>
              <span>{summary ? `New ${forge.label} token` : `${forge.label} token`}</span>
              <input
                type="password"
                autocomplete="new-password"
                spellcheck="false"
                bind:value={token[forge.forge]}
              />
            </label>
            <fieldset>
              <legend>Channels</legend>
              {#each store.channels as channel (channel.name)}
                <label class="channel-choice">
                  <input
                    type="checkbox"
                    checked={selected[forge.forge].includes(channel.name)}
                    onchange={(event) =>
                      toggleChannel(forge.forge, channel.name, event.currentTarget.checked)}
                  />
                  {channel.name}{channel.archived ? ' · archived' : ''}
                </label>
              {/each}
            </fieldset>
            {#if peerCopies}
              <small class="warn">Saved copies on peer nodes do not update automatically.</small>
            {/if}
            <div class="actions">
              <button
                class="btn p"
                onclick={() => save(forge.forge)}
                disabled={!token[forge.forge].trim() || selected[forge.forge].length === 0 || busy !== null}
              >{busy === forge.forge ? 'Saving…' : 'Save token'}</button>
              <button class="lnk" onclick={() => cancelEdit(forge.forge)} disabled={busy !== null}>Cancel</button>
            </div>
          </div>
        {:else if confirming === forge.forge}
          <div class="confirmation">
            <strong>Remove {forge.label} token?</strong>
            <span>This removes only the source on this node.</span>
            {#if peerCopies}<span class="warn">Saved copies on peer nodes will remain unchanged.</span>{/if}
            <div class="actions">
              <button class="btn d" onclick={() => remove(forge.forge)} disabled={busy !== null}
                >{busy === forge.forge ? 'Removing…' : 'Remove token'}</button
              >
              <button class="lnk" onclick={() => (confirming = null)} disabled={busy !== null}>Cancel</button>
            </div>
          </div>
        {:else}
          <div class="actions">
            <button class="lnk" onclick={() => edit(forge.forge, summary)} disabled={busy !== null}
              >{summary ? 'Replace' : 'Add token'}</button
            >
            {#if summary}
              <button class="lnk d" onclick={() => (confirming = forge.forge)} disabled={busy !== null}>Remove</button>
            {/if}
          </div>
        {/if}
        {#if notices[forge.forge]}<small class="ok" role="status">{notices[forge.forge]}</small>{/if}
        {#if errors[forge.forge]}<small class="bad" role="alert">{errors[forge.forge]}</small>{/if}
      </div>
    </div>
  {/each}
</div>

<details class="generic-import">
  <summary>Import another credential</summary>
  <label>
    <span>Credential TOML</span>
    <textarea
      bind:value={toml}
      rows="6"
      spellcheck="false"
      placeholder={'[credentials.name]\nchannels = ["personal"]\n[credentials.name.env]\nTOKEN = "…"'}
    ></textarea>
    <small>Sealed on arrival under this node's identity. Share a peer-bound credential from Nodes.</small>
  </label>
  <div class="actions">
    <button class="btn" onclick={importCredential} disabled={!toml.trim() || importing}>
      {importing ? 'Sealing…' : 'Import credential'}
    </button>
    {#if imported.length}<small class="ok">Sealed {imported.join(', ')}.</small>{/if}
  </div>
  {#if importError}<small class="bad" role="alert">{importError}</small>{/if}
</details>

<style>
  .forge-list {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  .forge-row {
    display: grid;
    grid-template-columns: 3px 118px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
  }
  .bar {
    align-self: stretch;
    background: var(--s3);
  }
  .bar.saved {
    background: var(--ok);
  }
  .forge-name,
  .forge-state,
  .editor,
  .confirmation {
    display: flex;
    flex-direction: column;
    gap: 5px;
    min-width: 0;
  }
  .forge-name small,
  .forge-state,
  .forge-state small {
    font: 12px var(--mono);
  }
  .forge-name small,
  .dim {
    color: var(--dim);
  }
  .editor {
    margin-top: 4px;
  }
  .editor > label {
    max-width: 34rem;
  }
  fieldset {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 16px;
    border: 0;
    padding: 0;
    margin: 2px 0;
  }
  legend {
    width: 100%;
    margin-bottom: 4px;
    color: var(--dim);
  }
  .channel-choice {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 6px;
  }
  .actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px 12px;
  }
  .generic-import {
    margin-top: 12px;
  }
  .generic-import summary {
    cursor: pointer;
    color: var(--acc);
  }
  .generic-import > label {
    margin-top: 10px;
  }
  .generic-import textarea {
    width: 100%;
  }
  .warn {
    color: var(--wait);
  }
  .ok {
    color: var(--ok);
  }
  .bad {
    color: var(--crit);
  }
  @media (max-width: 700px) {
    .forge-row {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 4px 12px;
    }
    .forge-state {
      grid-column: 2;
    }
    .editor > label {
      max-width: none;
    }
  }
</style>
