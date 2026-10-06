<script lang="ts">
  // What an agent showed of its work (`show_work`): a written account, its own
  // page, or plain files. The account is the agent's; the commit, the stale
  // mark and the checks beside it are the node's. A page only ever renders on
  // the isolated preview origin, sandboxed and with no network.
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { renderWithoutFetching } from '../lib/markdown'
  import type { ShownWork } from '../lib/types'

  let {
    items,
    onReview = false,
  }: {
    items: ShownWork[]
    /** On a review, staleness is against its candidate; a session has none. */
    onReview?: boolean
  } = $props()

  /** Preview URLs for the pages opened in place, by shown-work id. */
  let opened = $state<Record<string, string>>({})
  let failed = $state<Record<string, string>>({})

  async function open(item: ShownWork) {
    if (!item.document_slug) return
    try {
      const preview = await api.previewDoc(item.channel, item.document_slug)
      opened[item.id] = preview.url
    } catch (cause) {
      failed[item.id] = cause instanceof Error ? cause.message : String(cause)
    }
  }

  function size(bytes: number): string {
    if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`
    if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`
    return `${bytes} B`
  }

  function passed(check: ShownWork['checks'][number]): boolean {
    return check.outcome === 'passed' || (check.outcome === 'reused' && check.source_outcome === 'passed')
  }
</script>

{#if items.length}
  <section class="shown">
    <div class="h4">
      Shown work
      <b>· the agent's account, to help you decide · not verification · it does not go to the forge</b>
    </div>
    {#each items as item (item.id)}
      <article class:stale={onReview && item.stale}>
        <header>
          <b>{item.title}</b>
          <span class="tag">{item.format === 'markdown' ? 'account' : item.format === 'html' ? 'page' : 'files'}</span>
          <span class="mono" title={item.head_sha}>at {item.head_sha.slice(0, 12)}</span>
          {#if onReview && item.stale}
            <span class="chip warn" title={item.stale_reason ?? ''}>stale</span>
          {/if}
          <span class="mono dim">{formatAge(item.created_ms, clock.now)}</span>
        </header>
        {#if onReview && item.stale && item.stale_reason}
          <p class="dim">{item.stale_reason} · ask for it to be shown again if it matters</p>
        {/if}
        {#if item.markdown.trim()}
          <div class="md">{@html renderWithoutFetching(item.markdown)}</div>
        {/if}
        {#if item.document_slug}
          <div class="actions">
            {#if item.format === 'html' && !opened[item.id]}
              <button class="lnk" onclick={() => open(item)}>Open here</button>
            {/if}
            <a href="/docs/{item.channel}/{item.document_slug}/preview">{item.format === 'html' ? 'Open full size' : 'Open the files'}</a>
          </div>
          {#if failed[item.id]}
            <p class="crit">Could not load the isolated preview: {failed[item.id]}</p>
          {/if}
          {#if opened[item.id]}
            <iframe title={item.title} src={opened[item.id]} sandbox="allow-scripts"></iframe>
          {/if}
          <details>
            <summary>{item.files.length} {item.files.length === 1 ? 'file' : 'files'}</summary>
            <ul>
              {#each item.files as file (file.path)}
                <li><code>{file.path}</code> <small>{size(file.size_bytes)}</small></li>
              {/each}
            </ul>
          </details>
        {/if}
        <p class="checks">
          {#if item.checks.length}
            checks the node ran at this commit:
            {#each item.checks as check, i (i)}
              <span class:ok={passed(check)} class:bad={!passed(check) && check.outcome !== 'running'}>{passed(check) ? '✓' : check.outcome === 'running' ? '…' : '✗'} {check.command ?? 'command not recorded'}</span>
            {/each}
          {:else}
            no required checks ran at this commit
          {/if}
        </p>
      </article>
    {/each}
  </section>
{/if}

<style>
  .shown {
    display: flex;
    flex-direction: column;
    gap: 9px;
  }
  article {
    min-width: 0;
    padding: 10px;
    border-radius: 4px;
    background: var(--s1);
    font-size: 12.5px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  article.stale {
    border-left: 3px solid var(--wait);
  }
  header {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
  }
  .mono {
    font: 11.5px var(--mono);
    color: var(--ink2);
  }
  .dim,
  small {
    color: var(--dim);
  }
  p {
    margin: 0;
  }
  .crit {
    color: var(--crit);
  }
  .md {
    overflow-wrap: anywhere;
  }
  .md :global(p),
  .md :global(ul),
  .md :global(ol) {
    margin: 4px 0;
  }
  .md :global(pre) {
    overflow: auto;
    font: 11px/1.45 var(--mono);
  }
  .md :global(.no-fetch) {
    color: var(--dim);
    font: 11.5px var(--mono);
  }
  .actions {
    display: flex;
    gap: 12px;
    font: 12px var(--mono);
  }
  iframe {
    width: 100%;
    height: 60vh;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: white;
  }
  details summary {
    cursor: pointer;
    color: var(--ink2);
    font: 11.5px var(--mono);
  }
  ul {
    margin: 4px 0 0;
    padding-left: 18px;
  }
  code {
    overflow-wrap: anywhere;
  }
  .checks {
    font: 11.5px var(--mono);
    color: var(--dim);
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .checks .ok {
    color: var(--ok);
  }
  .checks .bad {
    color: var(--crit);
  }
</style>
