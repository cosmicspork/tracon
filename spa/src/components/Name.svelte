<script lang="ts">
  // A record as the operator reads it: its generated name (or a label the
  // caller gives, such as a node's own name or a policy's `vN`), the raw id one
  // hover away and one click from the clipboard.
  import { generatedName, type NameKind } from '../lib/names'

  let {
    kind,
    id,
    label,
    href,
    prefix = '',
    class: linkClass = '',
    hint,
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
  } = $props()

  const shown = $derived(label ?? (kind ? generatedName(kind, id) : id))
  const tip = $derived(hint ? `${hint} · ${id}` : id)
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

<span class="name" data-testid="name" data-id={id}
  >{#if href}<a class={linkClass || undefined} {href} title={tip}>{prefix}{shown}</a>{:else}{prefix}<span class="n" title={tip}>{shown}</span>{/if}<button
    type="button"
    class="copy"
    title={copied ? 'Copied' : `Copy id ${id}`}
    aria-label={copied ? 'Id copied' : `Copy the id of ${shown}`}
    onclick={copy}>{copied ? '✓' : '⧉'}</button
  ></span
>

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
</style>
