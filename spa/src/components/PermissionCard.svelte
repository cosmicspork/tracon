<script lang="ts">
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge, formatExpiry } from '../lib/format'
  import { chipLabel, nodeById, unreachableReason } from '../lib/nodes'
  import { editableFields, editedArguments } from '../lib/permission'
  import { permissionOptions, type Permission } from '../lib/types'
  import { store } from '../lib/store.svelte'

  let { permission, inline = false }: { permission: Permission; inline?: boolean } = $props()

  let busy = $state(false)
  let error = $state<string | null>(null)
  let drafts = $state<Record<string, string>>({})

  const options = $derived(permissionOptions(permission))
  const fields = $derived(editableFields(permission))
  const owner = $derived(nodeById(store.nodes, permission.node_id))
  const held = $derived(unreachableReason(store.nodes, store.mesh, permission.node_id))
  const request = $derived.by(() => {
    if (!permission.raw_input) return null
    try {
      return JSON.stringify(JSON.parse(permission.raw_input), null, 2)
    } catch {
      return permission.raw_input
    }
  })
  const command = $derived.by(() => {
    try {
      const input = JSON.parse(permission.raw_input ?? 'null')
      return typeof input?.command === 'string' ? input.command : null
    } catch {
      return null
    }
  })

  async function answer(optionId: string) {
    busy = true
    error = null
    try {
      const edited = optionId === 'allow_once' ? editedArguments(permission, drafts) : undefined
      await api.answer(permission.id, optionId, edited)
      await store.refetch()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }

  function label(kind: string, name: string): string {
    // Only the two once-options are offered: "always" is a policy decision the
    // gate does not delegate to a card.
    return { allow_once: 'Allow', reject_once: 'Deny' }[kind] ?? name
  }
</script>

<div class="card" class:inline class:held={held !== null}>
  <span class="bar"></span>
  {#if !inline}
    <span class="mono head">{formatAge(permission.created_ms, clock.now)}</span>
  {/if}
  <span class="t">
    <em>Permission</em>
    {permission.title}
    <small
      ><span class="chip" class:self={owner?.is_self} class:off={held !== null}>{chipLabel(store.nodes, permission.node_id)}</span> · {permission.kind ?? 'tool'} · {formatExpiry(permission.expires_ms, clock.now)}{command &&
      command !== permission.title
        ? ` · ${command}`
        : ''}{error ? ` · ${error}` : ''}</small
    >
  </span>
  <span class="act">
    {#if held !== null}
      <span class="why">{held} · cannot be decided until it returns</span>
    {:else}
      {#each options.filter((o) => o.kind === 'reject_once') as o (o.option_id)}
        <button class="lnk d" disabled={busy} onclick={() => answer(o.option_id)}
          >{label(o.kind, o.name)}</button
        >
      {/each}
      {#each options.filter((o) => o.kind === 'allow_once') as o (o.option_id)}
        <button class="btn p" disabled={busy} onclick={() => answer(o.option_id)}
          >{label(o.kind, o.name)}</button
        >
      {/each}
    {/if}
  </span>
  {#if fields.length && held === null}
    <div class="fields">
      {#each fields as f (f.key)}
        <label>
          <span>{f.key}</span>
          <textarea
            value={f.value}
            disabled={busy}
            spellcheck="true"
            oninput={(e) => (drafts[f.key] = e.currentTarget.value)}
          ></textarea>
        </label>
      {/each}
    </div>
  {/if}
  {#if request}
    <details class="request">
      <summary>Full request</summary>
      <pre>{request}</pre>
    </details>
  {/if}
</div>

<style>
  .card {
    display: grid;
    grid-template-columns: 3px 72px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
  }
  .card.inline {
    grid-template-columns: 3px minmax(0, 1fr) auto;
    border-radius: 0 4px 4px 0;
  }
  .request {
    grid-column: 3 / -1;
    min-width: 0;
    margin-top: 6px;
    color: var(--dim);
  }
  .inline .request {
    grid-column: 2 / -1;
  }
  .fields {
    grid-column: 3 / -1;
    display: grid;
    gap: 8px;
    margin-top: 8px;
    min-width: 0;
  }
  .inline .fields {
    grid-column: 2 / -1;
  }
  .fields label {
    display: grid;
    gap: 3px;
  }
  .fields span {
    font: 11px var(--mono);
    color: var(--dim);
  }
  .fields textarea {
    width: 100%;
    min-height: 5.5em;
    resize: vertical;
    box-sizing: border-box;
    font: 12.5px/1.45 var(--mono);
    color: var(--ink);
    background: var(--s0);
    border: 1px solid transparent;
    border-radius: 3px;
    padding: 8px;
  }
  .fields textarea:focus {
    outline: none;
    border-color: var(--wait);
  }
  .request summary {
    cursor: pointer;
    font: 11px var(--mono);
  }
  .request pre {
    max-height: 280px;
    overflow: auto;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: var(--ink);
    background: var(--s0);
    padding: 8px;
    border-radius: 3px;
  }
  .bar {
    align-self: stretch;
    background: var(--wait);
    border-radius: 2px 0 0 2px;
  }
  .head {
    color: var(--dim);
  }
  .t {
    font-weight: 500;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
    color: var(--wait);
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    margin-top: 2px;
  }
  .act {
    display: flex;
    gap: 12px;
    align-items: center;
    white-space: nowrap;
  }
  .why {
    font: 11.5px var(--mono);
    color: var(--dim);
  }
  /* Held: the owner cannot be reached, so nothing here is on you yet. */
  .card.held {
    background: linear-gradient(90deg, var(--wash-dim), var(--s1) 42%);
  }
  .card.held .bar {
    background: var(--dim);
  }
  .card.held .t em {
    color: var(--dim);
  }
  .t small .chip {
    vertical-align: baseline;
  }
  @media (max-width: 700px) {
    .card {
      grid-template-columns: 3px minmax(0, 1fr) auto;
    }
    .head {
      display: none;
    }
  }
</style>
