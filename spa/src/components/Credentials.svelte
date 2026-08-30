<script lang="ts">
  // What the broker holds — names, kinds, bindings, key names, never a value —
  // and a Share that hands one to another member, sealed to it over the hub.
  // The step that used to need a terminal on the sharing node.
  import { api } from '../lib/api'
  import { store } from '../lib/store.svelte'
  import type { CredentialSummary } from '../lib/types'

  let creds = $state<CredentialSummary[]>([])
  let to = $state<Record<string, string>>({})
  let busy = $state<string | null>(null)
  let errors = $state<Record<string, string>>({})
  let shared = $state<Record<string, string>>({})

  let version = $state(0)
  $effect(() => {
    void version
    api
      .credentials()
      .then((d) => (creds = d.credentials))
      .catch(() => (creds = []))
  })

  const peers = $derived(store.nodes.filter((n) => !n.is_self && n.reachable))

  async function share(c: CredentialSummary) {
    const target = to[c.name]
    if (!target || busy) return
    busy = c.name
    errors = { ...errors, [c.name]: '' }
    try {
      await api.shareCredential(c.name, target)
      shared = {
        ...shared,
        [c.name]: store.nodes.find((n) => n.id === target)?.name ?? target,
      }
      version += 1
    } catch (e) {
      errors = { ...errors, [c.name]: e instanceof Error ? e.message : String(e) }
    } finally {
      busy = null
    }
  }
</script>

{#if creds.length > 0}
  <div class="h4">Credentials <b>{creds.length} sealed · names and bindings only, never values</b></div>
  <div class="rows">
    {#each creds as c (c.name)}
      <div class="cred">
        <span class="bar"></span>
        <span class="nm">
          {c.name}
          <small>{c.kind}{c.provider ? ` · ${c.provider}` : ''}{c.identity ? ` · ${c.identity}` : ''}</small>
        </span>
        <span class="st">
          <span
            >{c.channels.length ? c.channels.join(', ') : 'no channel — unusable until bound'} ·
            {c.nodes.length === 0
              ? 'this node only'
              : `${c.nodes.length} node${c.nodes.length === 1 ? '' : 's'}`} · {c.env_keys.join(', ')}</span
          >
          {#if peers.length > 0}
            <span class="share">
              <select bind:value={to[c.name]}>
                <option value="" disabled selected>Share to…</option>
                {#each peers as p (p.id)}
                  <option value={p.id} disabled={c.nodes.includes(p.id)}
                    >{p.name}{c.nodes.includes(p.id) ? ' · has it' : ''}</option
                  >
                {/each}
              </select>
              <button class="lnk" onclick={() => share(c)} disabled={busy === c.name || !to[c.name]}
                >{busy === c.name ? 'Sharing…' : 'Share'}</button
              >
              {#if shared[c.name]}<small class="ok">handed to {shared[c.name]}</small>{/if}
            </span>
          {/if}
          {#if errors[c.name]}<span class="err">{errors[c.name]}</span>{/if}
        </span>
      </div>
    {/each}
  </div>
{/if}

<style>
  .h4 {
    margin-top: 8px;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .cred {
    display: grid;
    grid-template-columns: 3px 150px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    padding: 9px 14px 9px 0;
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
  .st > span {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .share {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .share select {
    background: var(--s2);
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
