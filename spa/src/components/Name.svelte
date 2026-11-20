<script lang="ts">
  // A record as the operator reads it: its generated name (or a label the
  // caller gives, such as a node's own name or a policy's `vN`), the raw id one
  // hover away and one click from the clipboard.
  //
  // With `renamable`, it also renames the record in place: Enter saves, Esc
  // cancels, and an empty name puts the generated one back. The name shows
  // on every node, since names replicate.
  import { tick } from 'svelte'

  import { api } from '../lib/api'
  import { applyOperatorName, generatedName, nameOf, nameProblem, operatorName, type NameKind } from '../lib/names'

  let {
    kind,
    id,
    label,
    href,
    prefix = '',
    class: linkClass = '',
    hint,
    renamable = false,
    channel,
  }: {
    /** Names the record; leave out only with `label`. */
    kind?: NameKind
    /** The full raw id: the tooltip and what is copied. */
    id: string
    /** Shown in place of the generated name. */
    label?: string
    href?: string
    /** Text inside the link before the name, such as "continued as ". */
    prefix?: string
    /** The link's class, where the page styles its links. */
    class?: string
    /** Said on hover before the id. */
    hint?: string
    /** Offer an inline rename. Needs `kind`. */
    renamable?: boolean
    /** The record's channel, for a manifest the node may not have recorded yet. */
    channel?: string
  } = $props()

  const own = $derived(kind ? operatorName(kind, id) : undefined)
  const shown = $derived(label ?? (kind ? nameOf(kind, id) : id))
  const tip = $derived.by(() => {
    const parts = [hint, own && kind ? generatedName(kind, id) : undefined, id].filter(Boolean)
    return parts.join(' · ')
  })

  let editing = $state(false)
  let draft = $state('')
  let saving = $state(false)
  let problem = $state<string | null>(null)
  let input = $state<HTMLInputElement | null>(null)

  async function startEdit(event: MouseEvent) {
    event.preventDefault()
    event.stopPropagation()
    draft = own ?? ''
    problem = null
    editing = true
    await tick()
    input?.focus()
    input?.select()
  }

  function cancel() {
    editing = false
    problem = null
  }

  async function save() {
    if (!kind || saving) return
    const next = draft.trim()
    if (next === (own ?? '')) return cancel()
    if (next) {
      const why = nameProblem(next)
      if (why) {
        problem = why
        return
      }
    }
    saving = true
    problem = null
    try {
      const done = next ? await api.rename(kind, id, next, channel) : await api.unname(kind, id, channel)
      applyOperatorName(done.kind, done.id, done.name === done.generated ? null : done.name)
      editing = false
    } catch (err) {
      problem = err instanceof Error ? err.message : String(err)
    } finally {
      saving = false
    }
  }

  function onKey(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault()
      void save()
    } else if (event.key === 'Escape') {
      event.preventDefault()
      cancel()
    }
  }
  let copied = $state(false)
  let timer: ReturnType<typeof setTimeout> | undefined

  async function copy(event: MouseEvent) {
    event.preventDefault()
    event.stopPropagation()
    try {
      await navigator.clipboard.writeText(id)
      copied = true
      clearTimeout(timer)
      timer = setTimeout(() => (copied = false), 1200)
    } catch {
      // The id is in the tooltip to read by hand.
    }
  }
</script>

{#if editing && kind}<span class="name editing" data-testid="name" data-id={id}
    ><input
      bind:this={input}
      bind:value={draft}
      class="rename"
      maxlength="64"
      placeholder={generatedName(kind, id)}
      aria-label="New name; empty puts back {generatedName(kind, id)}"
      disabled={saving}
      onkeydown={onKey}
      onblur={() => !saving && !problem && cancel()}
    />{#if problem}<span class="problem" role="alert">{problem}</span>{/if}</span
  >{:else}<span class="name" data-testid="name" data-id={id}
    >{#if href}<a class={linkClass || undefined} {href} title={tip}>{prefix}{shown}</a>{:else}{prefix}<span class="n" title={tip}>{shown}</span>{/if}<button
      type="button"
      class="copy"
      title={copied ? 'Copied' : `Copy id ${id}`}
      aria-label={copied ? 'Id copied' : `Copy the id of ${shown}`}
      onclick={copy}>{copied ? '✓' : '⧉'}</button
    >{#if renamable && kind}<button
        type="button"
        class="copy edit"
        title="Rename"
        aria-label="Rename {shown}"
        data-testid="rename"
        onclick={startEdit}>✎</button
      >{/if}</span
  >{/if}

<style>
  .name {
    white-space: nowrap;
  }
  .n {
    font-family: var(--mono);
    font-size: 0.95em;
  }
  .copy {
    background: none;
    border: 0;
    padding: 0 0 0 0.25em;
    margin: 0;
    color: var(--dim);
    font: inherit;
    font-size: 0.85em;
    cursor: pointer;
    opacity: 0.55;
  }
  .name:hover .copy,
  .copy:focus-visible {
    opacity: 1;
  }
  .copy:hover {
    color: inherit;
  }
  .rename {
    font: inherit;
    font-family: var(--mono);
    font-size: 0.95em;
    min-width: 12em;
    max-width: 100%;
    padding: 0 0.25em;
  }
  .editing {
    white-space: normal;
  }
  .problem {
    display: block;
    color: var(--crit);
    font-size: 0.85em;
  }
</style>
