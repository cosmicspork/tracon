<script lang="ts">
  // A model list long enough to need a search: type to narrow it, arrows and
  // Enter to pick, Escape to leave it as it was. The models this node has
  // already run sit first; the harness's own order follows.
  import { filterModels, orderModels } from '../lib/models'
  import type { ModelOption } from '../lib/types'

  let {
    value = $bindable(''),
    models,
    recent = [],
    none = null,
    disabled = false,
    onchange,
  }: {
    value?: string
    models: ModelOption[]
    /** Model values used here before, newest first. */
    recent?: string[]
    /** When set, an empty value is a real choice and this is its label. */
    none?: string | null
    disabled?: boolean
    onchange?: (value: string) => void
  } = $props()

  let query = $state('')
  let open = $state(false)
  let active = $state(0)
  let input = $state<HTMLInputElement | null>(null)
  const listId = `mp-${Math.random().toString(36).slice(2, 8)}`

  const selected = $derived(models.find((m) => m.value === value) ?? null)
  const label = $derived(selected?.name ?? (value || (none ?? '')))
  const groups = $derived.by(() => {
    const { recent: used, rest } = orderModels(filterModels(models, query), recent)
    return { used, rest }
  })
  const rows = $derived<{ value: string; name: string; note?: string }[]>([
    ...(none !== null && query.trim() === '' ? [{ value: '', name: none }] : []),
    ...groups.used.map((m) => ({ ...m, note: 'recent' })),
    ...groups.rest,
    // A bound model the harness does not offer now, shown as itself rather
    // than as nothing: an empty box would read as no binding at all.
    ...(value && !selected && !query ? [{ value, name: value, note: 'not offered here now' }] : []),
  ])

  function show() {
    if (disabled) return
    query = ''
    active = Math.max(0, rows.findIndex((r) => r.value === value))
    open = true
  }
  function pick(v: string) {
    open = false
    query = ''
    if (v === value) return
    value = v
    onchange?.(v)
  }
  function onkeydown(e: KeyboardEvent) {
    if (!open) {
      if (e.key === 'ArrowDown' || e.key === 'Enter') {
        e.preventDefault()
        show()
      }
      return
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      active = Math.min(rows.length - 1, active + 1)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      active = Math.max(0, active - 1)
    } else if (e.key === 'Enter') {
      e.preventDefault()
      if (rows[active]) pick(rows[active].value)
    } else if (e.key === 'Escape') {
      e.preventDefault()
      open = false
      query = ''
    }
  }
  $effect(() => {
    void query
    active = 0
  })
</script>

<div class="mp" class:open>
  <input
    bind:this={input}
    value={open ? query : label}
    placeholder={open ? 'Type to search' : 'Choose a model'}
    spellcheck="false"
    autocomplete="off"
    role="combobox"
    aria-controls={listId}
    aria-expanded={open}
    aria-autocomplete="list"
    {disabled}
    onfocus={show}
    onclick={show}
    oninput={(e) => (query = (e.currentTarget as HTMLInputElement).value)}
    onblur={() => {
      open = false
      query = ''
    }}
    {onkeydown}
  />
  {#if open}
    <div class="list" role="listbox" id={listId}>
      {#each rows as r, i (r.value + (r.note ?? ''))}
        <!-- mousedown, not click: the input's blur would close the list first. -->
        <button
          type="button"
          role="option"
          aria-selected={r.value === value}
          class:active={i === active}
          class:on={r.value === value}
          onmousedown={(e) => {
            e.preventDefault()
            pick(r.value)
          }}
          onmouseenter={() => (active = i)}
        >
          <span>{r.name}</span>
          {#if r.note}<small>{r.note}</small>{:else if r.value && r.value !== r.name}<small>{r.value}</small>{/if}
        </button>
      {:else}
        <div class="none">No model matches</div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .mp {
    position: relative;
    min-width: 0;
  }
  input {
    width: 100%;
    box-sizing: border-box;
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
  }
  input:disabled {
    opacity: 0.5;
  }
  .list {
    position: absolute;
    z-index: 5;
    top: calc(100% + 3px);
    left: 0;
    right: 0;
    max-height: 260px;
    overflow: auto;
    background: var(--s2);
    border-radius: 4px;
    padding: 4px;
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.25);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .list button {
    display: flex;
    gap: 10px;
    align-items: baseline;
    justify-content: space-between;
    padding: 6px 8px;
    border-radius: 3px;
    font: 13px var(--sans);
    color: var(--ink);
    background: none;
    border: 0;
    text-align: left;
    cursor: pointer;
    min-width: 0;
  }
  .list button span {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .list button small {
    font: 11px var(--mono);
    color: var(--dim);
    white-space: nowrap;
  }
  .list button.active {
    background: var(--s3);
  }
  .list button.on span {
    color: var(--acc);
  }
  .none {
    padding: 6px 8px;
    font: 12.5px var(--sans);
    color: var(--dim);
  }
  @media (max-width: 700px) {
    input {
      font-size: 16px;
    }
  }
</style>
