<script lang="ts">
  // What the broker holds — names, kinds, bindings, key names, never a value —
  // and a Share that hands one to another member, sealed to it over the hub.
  // The step that used to need a terminal on the sharing node. Importing one
  // from TOML sits at the top, since it takes any credential, not only a
  // forge token.
  import { api } from '../lib/api'
  import { autogrow } from '../lib/autogrow'
  import { store } from '../lib/store.svelte'
  import { credentialReach } from '../lib/credentials'
  import type { CredentialSummary } from '../lib/types'
  import Card from './settings/Card.svelte'

  let creds = $state<CredentialSummary[]>([])
  let loaded = $state(false)
  let to = $state<Record<string, string>>({})
  let busy = $state<string | null>(null)
  let errors = $state<Record<string, string>>({})
  let shared = $state<Record<string, string>>({})
  let confirming = $state<CredentialSummary | null>(null)
  let toml = $state('')
  let importing = $state(false)
  let importError = $state('')
  let imported = $state<string[]>([])

  // `version` moves when the Forge tokens card saves or removes one;
  // `onchange` tells it this card imported or shared one. A share changes
  // the peer copies its rows warn about.
  let { version = 0, onchange }: { version?: number; onchange?: () => void } = $props()

  let local = $state(0)
  $effect(() => {
    void version
    void local
    api
      .credentials()
      .then((d) => (creds = d.credentials))
      .catch(() => (creds = []))
      .finally(() => (loaded = true))
  })

  const peers = $derived(store.nodes.filter((n) => !n.is_self && n.reachable))

  function reviewShare(c: CredentialSummary) {
    if (!to[c.name] || busy) return
    errors = { ...errors, [c.name]: '' }
    confirming = c
  }

  async function share(c: CredentialSummary) {
    const target = to[c.name]
    if (!target || busy) return
    busy = c.name
    errors = { ...errors, [c.name]: '' }
    const updating = c.nodes.includes(target)
    try {
      await api.shareCredential(c.name, target)
      const node = store.nodes.find((candidate) => candidate.id === target)?.name ?? target
      shared = {
        ...shared,
        [c.name]: updating ? `updated copy on ${node}` : `handed to ${node}`,
      }
      confirming = null
      if (onchange) onchange()
      else local += 1
    } catch (e) {
      errors = { ...errors, [c.name]: e instanceof Error ? e.message : String(e) }
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
      if (onchange) onchange()
      else local += 1
    } catch (caught) {
      importError = caught instanceof Error ? caught.message : String(caught)
    } finally {
      importing = false
    }
  }
</script>

<Card title="Credentials" note="Credentials on the serving node, by name. Sharing seals a copy to one peer.">
  {#snippet actions()}<span class="count">{creds.length} held</span>{/snippet}
  <details class="import">
    <summary>Import credential</summary>
    <label>
      <span>Credential TOML</span>
      <textarea
        class="grow"
        bind:value={toml}
        use:autogrow={toml}
        rows="6"
        spellcheck="false"
        placeholder={'[credentials.name]\nchannels = ["personal"]\n[credentials.name.env]\nTOKEN = "…"'}
      ></textarea>
      <small>Sealed on arrival. Share to a peer from its row.</small>
    </label>
    <div class="import-actions">
      <button class="btn" onclick={importCredential} disabled={!toml.trim() || importing}>
        {importing ? 'Sealing…' : 'Import credential'}
      </button>
      {#if imported.length}<small class="ok">Sealed {imported.join(', ')}.</small>{/if}
    </div>
    {#if importError}<small class="err" role="alert">{importError}</small>{/if}
  </details>
  {#if creds.length > 0}
  <div class="rows">
    {#each creds as c (c.name)}
      <div class="cred">
        <span class="bar"></span>
        <span class="nm">
          {c.name}
          <small>{c.kind}{c.provider ? ` · ${c.provider}` : ''}{c.identity ? ` · ${c.identity}` : ''}</small>
        </span>
        <span class="st">
          <span>
            {c.channels.length ? c.channels.join(', ') : 'no channel — unusable until bound'} ·
            {c.nodes.length === 0 ? 'serving node only' : `${c.nodes.length} peer cop${c.nodes.length === 1 ? 'y' : 'ies'}`} ·
            {c.env_keys.join(', ')}
          </span>
          {#if credentialReach(c)}
            {@const reach = credentialReach(c)}
            <span title={reach}>{reach}</span>
          {/if}
          {#if peers.length > 0}
            {@const target = to[c.name]}
            {@const targetName = peers.find((node) => node.id === target)?.name ?? target}
            <span class="share">
              <select bind:value={to[c.name]}>
                <option value="" disabled selected>Share sealed copy to…</option>
                {#each peers as p (p.id)}
                  <option value={p.id}>{p.name}{c.nodes.includes(p.id) ? ' · has a copy' : ''}</option>
                {/each}
              </select>
              {#if confirming?.name === c.name}
                <span class="confirm">
                  Send {c.name} sealed to {targetName}? {c.nodes.includes(target) ? 'It replaces that peer’s saved copy.' : 'It creates a new saved copy there.'} The serving-node source remains unchanged.
                  <button class="btn p" onclick={() => share(c)} disabled={busy === c.name}>{busy === c.name ? 'Sending…' : 'Confirm sealed share'}</button>
                  <button class="lnk" onclick={() => (confirming = null)} disabled={busy === c.name}>Cancel</button>
                </span>
              {:else}
                <button class="lnk" onclick={() => reviewShare(c)} disabled={busy === c.name || !target}>Review share</button>
              {/if}
              {#if shared[c.name]}<small class="ok">{shared[c.name]}</small>{/if}
            </span>
          {/if}
          {#if errors[c.name]}<span class="err">{errors[c.name]}</span>{/if}
        </span>
      </div>
    {/each}
  </div>
  {:else if loaded}
  <p class="empty">No credentials held yet.</p>
  {/if}
</Card>

<style>
  .count {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .import summary {
    cursor: pointer;
    color: var(--acc);
  }
  .import > label {
    margin-top: 10px;
  }
  .import textarea {
    width: 100%;
  }
  .import-actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px 12px;
    margin-top: 8px;
  }
  .empty {
    margin: 10px 0 0;
    font-size: 12.5px;
    color: var(--dim);
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .cred {
    display: grid;
    grid-template-columns: 3px 140px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s2);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
  }
  .bar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--s3);
  }
  .nm {
    font-weight: 600;
    min-width: 0;
  }
  .nm small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    font-weight: 400;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .st {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font: 12.5px var(--mono);
    color: var(--ink2);
    min-width: 0;
  }
  .confirm {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 10px;
    color: var(--ink2);
    white-space: normal;
  }
  .st > span {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .share {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
    white-space: normal;
  }
  .share select {
    background: var(--s1);
    border: 0;
    border-radius: 3px;
    color: var(--ink);
    padding: 4px 8px;
    font: 12.5px var(--sans);
  }
  .lnk {
    background: none;
    border: 0;
    padding: 0;
    font: 12.5px var(--sans);
    color: var(--acc);
    cursor: pointer;
  }
  .lnk:disabled {
    color: var(--dim);
    cursor: default;
  }
  .ok {
    color: var(--ok);
    font: 11.5px var(--mono);
  }
  .err {
    color: var(--crit);
  }
  @media (max-width: 700px) {
    .cred {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 4px 12px;
    }
    .st {
      grid-column: 2;
    }
    .share {
      flex-wrap: wrap;
    }
  }
</style>
