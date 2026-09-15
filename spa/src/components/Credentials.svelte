<script lang="ts">
  // What the broker holds — names, kinds, bindings, key names, never a value —
  // and a Share that hands one to another member, sealed to it over the hub.
  // The step that used to need a terminal on the sharing node.
  import { api } from '../lib/api'
  import { store } from '../lib/store.svelte'
  import type { CredentialSummary } from '../lib/types'
  import Card from './settings/Card.svelte'

  let creds = $state<CredentialSummary[]>([])
  let to = $state<Record<string, string>>({})
  let busy = $state<string | null>(null)
  let errors = $state<Record<string, string>>({})
  let shared = $state<Record<string, string>>({})
  let confirming = $state<CredentialSummary | null>(null)

  let version = $state(0)
  $effect(() => {
    void version
    api
      .credentials()
      .then((d) => (creds = d.credentials))
      .catch(() => (creds = []))
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
      version += 1
    } catch (e) {
      errors = { ...errors, [c.name]: e instanceof Error ? e.message : String(e) }
    } finally {
      busy = null
    }
  }
</script>

{#if creds.length > 0}
  <Card title="Credential copies" note="Every credential the serving node holds, by name and key only. Sharing sends a sealed copy to one named peer; no value is ever read back.">
  {#snippet actions()}<span class="count">{creds.length} held</span>{/snippet}
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
  </Card>
{/if}

<style>
  .count {
    font: 12px var(--mono);
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
