<script lang="ts">
  // What an approval publishes to the forge as text — the change's title
  // and description, and the comment — shown as the forge renders it, or as
  // the exact source it is sent as. The node composes it with the function
  // the approval itself calls, so anything the node adds (the opener's fold
  // under `[review] generated_details`) is in it; it is asked again as the
  // fields above are edited. A review another node holds is composed here by
  // the same rules, since this screen cannot ask its owner.
  import { api } from '../lib/api'
  import { codeSpans, renderForge } from '../lib/markdown'
  import { localOutgoing, sameOutgoing, type LocalInputs, type Outgoing, type OutgoingAsked } from '../lib/outgoing'

  let {
    id,
    asked,
    local,
    remote,
    noun,
  }: {
    id: string
    /** The approval as the screen would send it now. */
    asked: OutgoingAsked
    /** The same, for composing it here when the node cannot answer. */
    local: LocalInputs
    /** The review is another node's: compose it here. */
    remote: boolean
    noun: string
  } = $props()

  /** How long typing rests before the node is asked again. */
  const SETTLE_MS = 250

  let view = $state<'preview' | 'source'>('preview')
  let fromNode = $state<Outgoing | null>(null)
  /** The node could not answer for it (an older node, a fixture), so it is composed here. */
  let unanswered = $state(false)
  let asking = $state(false)
  let sent = 0

  const here = $derived(localOutgoing(local))
  // While the node is asked again the text is composed here, by its rules,
  // so the preview follows each keystroke; the node's answer then replaces it.
  const shown = $derived(remote || unanswered || asking || !fromNode ? here : fromNode)
  const key = $derived(JSON.stringify(asked))

  $effect(() => {
    if (remote) return
    const body = JSON.parse(key) as OutgoingAsked
    const n = ++sent
    asking = true
    const t = setTimeout(async () => {
      try {
        const answer = await api.reviewOutgoing(id, body)
        if (n !== sent) return
        if (!sameOutgoing(answer, fromNode)) fromNode = answer
        unanswered = false
      } catch {
        if (n === sent) unanswered = true
      } finally {
        if (n === sent) asking = false
      }
    }, SETTLE_MS)
    return () => clearTimeout(t)
  })

  const how = $derived(
    remote
      ? 'composed here as its node will · edit above'
      : unanswered
        ? 'composed here · the node did not answer'
        : asking
          ? 'composing…'
          : 'as the node will send it · edit above',
  )
</script>

<div class="out">
  <div class="head">
    <span class="h5">What the {noun} gets <b>{how}</b></span>
    <div class="tabs" role="tablist" aria-label="Show the outgoing text">
      <button role="tab" class:on={view === 'preview'} aria-selected={view === 'preview'} onclick={() => (view = 'preview')}
        >Preview</button
      >
      <button role="tab" class:on={view === 'source'} aria-selected={view === 'source'} onclick={() => (view = 'source')}
        >Source</button
      >
    </div>
  </div>
  {#if shown.refusal}
    <div class="note" role="status">Approving is refused as written: {shown.refusal}</div>
  {:else if shown.description}
    {#if view === 'preview'}
      <div class="card">
        <div class="title">{@html codeSpans(shown.description.title)}</div>
        {#if shown.description.body.trim()}
          <!-- Sanitized as the agent's other words are: nothing fetched, links open beside. -->
          <div class="md">{@html renderForge(shown.description.body)}</div>
        {:else}
          <div class="dim">No description.</div>
        {/if}
      </div>
    {:else}
      <div class="label">title</div>
      <pre data-testid="outgoing-title">{shown.description.title}</pre>
      <div class="label">body</div>
      <pre data-testid="outgoing-body">{shown.description.body}</pre>
    {/if}
  {:else}
    <div class="dim">No title or description is sent.</div>
  {/if}
  {#if shown.comment}
    <div class="label">comment</div>
    {#if view === 'preview'}
      <div class="card"><div class="md">{@html renderForge(shown.comment)}</div></div>
    {:else}
      <pre>{shown.comment}</pre>
    {/if}
  {/if}
</div>

<style>
  .out {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin: 6px 0;
  }
  .head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
  }
  .h5 {
    font-size: 12.5px;
    color: var(--ink);
  }
  .h5 b {
    font-weight: 400;
    color: var(--dim);
  }
  .tabs {
    display: flex;
    gap: 6px;
  }
  .tabs button {
    font: 12px var(--mono);
    padding: 2px 10px;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: var(--s1);
    color: var(--dim);
    cursor: pointer;
  }
  .tabs button.on {
    color: inherit;
    border-color: currentColor;
  }
  .card {
    border: 1px solid var(--rule);
    border-radius: 4px;
    padding: 9px 11px;
    font-size: 13.5px;
    overflow-wrap: anywhere;
  }
  .title {
    font-weight: 600;
    color: var(--ink);
    margin-bottom: 6px;
  }
  .title :global(code),
  .md :global(code) {
    font: 0.9em var(--mono);
    background: var(--s2);
    border-radius: 3px;
    padding: 0 3px;
  }
  .label {
    font: 11.5px var(--mono);
    color: var(--dim);
    margin-top: 4px;
  }
  pre {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font: 12px var(--mono);
    background: var(--s2);
    border-radius: 4px;
    padding: 8px 10px;
    margin: 0;
  }
  .note {
    font-size: 12.5px;
    color: var(--wait);
  }
  .dim {
    font-size: 12.5px;
    color: var(--dim);
  }
  .md {
    color: var(--ink2);
  }
  .md > :global(:first-child) {
    margin-top: 0;
  }
  .md > :global(:last-child) {
    margin-bottom: 0;
  }
  .md :global(p),
  .md :global(ul),
  .md :global(ol),
  .md :global(pre),
  .md :global(blockquote),
  .md :global(details) {
    margin: 4px 0;
  }
  .md :global(ul),
  .md :global(ol) {
    padding-left: 2.5ch;
  }
  .md :global(h1),
  .md :global(h2),
  .md :global(h3),
  .md :global(h4) {
    margin: 0.7em 0 0.2em;
    font-size: 1em;
    font-weight: 600;
    color: var(--ink);
  }
  .md :global(pre) {
    background: var(--s2);
    border-radius: 4px;
    padding: 8px 10px;
    overflow-x: auto;
  }
  .md :global(pre code) {
    background: none;
    padding: 0;
  }
  .md :global(blockquote) {
    border-left: 2px solid var(--rule);
    padding-left: 1.5ch;
  }
  .md :global(summary) {
    cursor: pointer;
    color: var(--ink);
  }
  .md :global(a) {
    color: var(--acc);
  }
  .md :global(.no-fetch) {
    color: var(--dim);
  }
</style>
