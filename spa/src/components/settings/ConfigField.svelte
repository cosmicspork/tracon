<script lang="ts">
  // One node.toml key, as the node describes it: the control its shape asks
  // for, whether a change applies now or at the next restart, and, for a key
  // this browser may not change, why not. A secret is written, never read
  // back: the field says whether one is held and offers to replace it.
  import { autogrow } from '../../lib/autogrow'
  import { changed, initialDraft, magnitude, parseDraft, shown, type Draft, type Field } from '../../lib/config-form'

  let {
    field,
    draft,
    errors = [],
    disabled = false,
    owed = false,
    onchange,
  }: {
    field: Field
    draft?: Draft
    errors?: string[]
    disabled?: boolean
    /** Saved, and waiting for the node to restart. */
    owed?: boolean
    onchange: (draft: Draft | undefined) => void
  } = $props()

  const id = $derived(`cfg-${field.key.replace(/[^A-Za-z0-9_-]+/g, '-')}`)
  const current = $derived(draft ?? initialDraft(field))
  const off = $derived(disabled || !field.editable)
  const dirty = $derived(changed(field, draft))
  const parsed = $derived(parseDraft(field, current))
  const hint = $derived(
    field.control === 'number' && typeof parsed.value === 'number' ? magnitude(parsed.value, field.unit) : '',
  )
  const described = $derived(`${id}-note`)

  function set(v: string | boolean) {
    onchange({ v })
  }
  function reset() {
    onchange({ v: field.control === 'bool' ? false : '', reset: true })
  }
  function undo() {
    onchange(undefined)
  }
</script>

<div class="field" class:wide={field.control === 'lines'} class:dirty data-key={field.key}>
  <div class="head">
    {#if field.control === 'bool'}
      <span class="name" id="{id}-label">{field.label}</span>
    {:else}
      <label class="name" for={id}>{field.label}</label>
    {/if}
    <span class="badge" class:live={field.spec.apply === 'live'} title={field.spec.apply === 'live' ? 'Applies to the running node' : 'Applies after the node restarts'}>
      {field.spec.apply === 'live' ? 'live' : owed ? 'restart owed' : 'needs restart'}
    </span>
  </div>
  <code class="key">{field.key}</code>

  {#if field.control === 'bool'}
    <label class="toggle">
      <input
        {id}
        type="checkbox"
        checked={current.v === true}
        disabled={off || current.reset}
        aria-labelledby="{id}-label"
        aria-describedby={described}
        onchange={(e) => set(e.currentTarget.checked)}
      />
      <span>{current.reset ? 'default' : current.v === true ? 'On' : 'Off'}</span>
    </label>
  {:else if field.control === 'select'}
    <select
      {id}
      value={String(current.v)}
      disabled={off || current.reset}
      aria-describedby={described}
      onchange={(e) => set(e.currentTarget.value)}
    >
      {#if field.optional}<option value="">none</option>{/if}
      {#if String(current.v) !== '' && !(field.choices ?? []).includes(String(current.v))}
        <option value={String(current.v)}>{current.v}</option>
      {/if}
      {#each field.choices ?? [] as choice (choice)}
        {#if choice !== '' || !field.optional}<option value={choice}>{choice || 'none'}</option>{/if}
      {/each}
    </select>
  {:else if field.control === 'lines'}
    <textarea
      class="grow"
      {id}
      use:autogrow={String(current.v)}
      rows={Math.min(6, Math.max(2, String(current.v).split('\n').length))}
      value={String(current.v)}
      disabled={off || current.reset}
      spellcheck="false"
      placeholder="one per line"
      aria-describedby={described}
      oninput={(e) => set(e.currentTarget.value)}
    ></textarea>
  {:else if field.control === 'secret'}
    <span class="secret">
      <span class="chip" class:off={!field.secretSet || current.reset}>{current.reset ? 'removed on save' : field.secretSet ? 'set' : 'not set'}</span>
    </span>
    <input
      {id}
      type="password"
      autocomplete="new-password"
      value={current.reset ? '' : String(current.v)}
      disabled={off || current.reset}
      placeholder={field.secretSet ? 'Replace with a new value' : 'Set a value'}
      aria-describedby={described}
      oninput={(e) => set(e.currentTarget.value)}
    />
  {:else}
    <span class="with-unit">
      <input
        {id}
        type="text"
        inputmode={field.control === 'number' ? (field.integer ? 'numeric' : 'decimal') : undefined}
        value={String(current.v)}
        disabled={off || current.reset}
        spellcheck="false"
        autocomplete="off"
        placeholder={field.optional ? 'none' : field.control === 'number' ? 'a number' : ''}
        aria-describedby={described}
        aria-invalid={errors.length > 0 || (dirty && !!parsed.error)}
        oninput={(e) => set(e.currentTarget.value)}
      />
      {#if field.unit}<span class="unit">{field.unit}</span>{/if}
    </span>
  {/if}

  <small id={described} class="note">
    {#if !field.editable && field.whyNot}
      <span class="why">{field.whyNot}</span>
    {:else}
      {#if hint}<span>{hint}.</span>{/if}
      {#if current.reset}
        <span>{field.control === 'secret' ? 'Removed from node.toml on save.' : 'Taken out of node.toml on save; the node’s default applies.'}</span>
      {:else if field.control !== 'secret'}
        <span>{field.written ? 'Set in node.toml.' : field.value === undefined ? 'Not set.' : 'The node’s default.'}</span>
      {/if}
      {#if dirty && !current.reset && field.control !== 'secret'}<span>Was {shown(field.value)}.</span>{/if}
      {#if field.help}<span>{field.help}</span>{/if}
    {/if}
  </small>
  {#if parsed.error && dirty}<small class="bad" role="alert">{parsed.error}</small>{/if}
  {#each errors as message (message)}<small class="bad" role="alert">{message}</small>{/each}

  {#if field.editable && !disabled}
    <div class="acts">
      {#if dirty || current.reset}
        <button class="lnk" type="button" onclick={undo}>Undo</button>
      {/if}
      {#if !current.reset && (field.control === 'secret' ? field.secretSet : field.written)}
        <button class="lnk" type="button" onclick={reset}>{field.control === 'secret' ? 'Remove' : 'Use default'}</button>
      {/if}
    </div>
  {/if}
</div>

<style>
  .field {
    display: grid;
    gap: 4px;
    align-content: start;
    min-width: 0;
    padding: 8px 10px;
    border-radius: 4px;
    background: var(--s2);
    box-shadow: inset 3px 0 0 transparent;
  }
  .field.dirty {
    box-shadow: inset 3px 0 0 var(--acc);
  }
  .field.wide {
    grid-column: 1 / -1;
  }
  .head {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    justify-content: space-between;
    gap: 2px 8px;
  }
  .name {
    font: 500 13.5px var(--sans);
    color: var(--ink);
  }
  .key {
    font: 11.5px var(--mono);
    color: var(--dim);
    overflow-wrap: anywhere;
  }
  .badge {
    font: 500 10.5px var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--wait);
    white-space: nowrap;
  }
  .badge.live {
    color: var(--ok);
  }
  input:not([type='checkbox']),
  select,
  textarea {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--mono);
    min-width: 0;
    width: 100%;
    box-sizing: border-box;
  }
  input:not([type='checkbox']),
  select {
    min-height: 36px;
  }
  select {
    font-family: var(--sans);
  }
  input:disabled,
  select:disabled,
  textarea:disabled {
    opacity: 0.55;
  }
  input[aria-invalid='true'] {
    box-shadow: inset 0 0 0 1px var(--crit);
  }
  .with-unit {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .unit {
    font-size: 12.5px;
    color: var(--dim);
    white-space: nowrap;
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 36px;
    color: var(--ink);
    font-size: 13.5px;
  }
  .toggle input {
    width: 18px;
    height: 18px;
    margin: 0;
  }
  .secret {
    display: flex;
  }
  small {
    font-size: 12.5px;
    color: var(--dim);
  }
  .note {
    display: flex;
    flex-wrap: wrap;
    gap: 0 4px;
  }
  .why {
    color: var(--ink2);
  }
  .bad {
    color: var(--crit);
  }
  .acts {
    display: flex;
    gap: 12px;
  }
  .acts:empty {
    display: none;
  }
</style>
