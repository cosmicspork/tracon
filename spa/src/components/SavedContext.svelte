<script lang="ts">
  // One context the session saved before a compaction or a restart of its
  // harness: its full-text conversation, what else it holds, and its export.
  // Loaded when it is opened, since a conversation can be long.
  import { api, savedContextExportHref } from '../lib/api'
  import { formatBytes } from '../lib/data'
  import type { SavedContext } from '../lib/types'

  let { sessionId, index }: { sessionId: string; index: number } = $props()

  let saved = $state<SavedContext | null>(null)
  let error = $state('')

  $effect(() => {
    let live = true
    api
      .savedContext(sessionId, index)
      .then((got) => {
        if (live) saved = got
      })
      .catch((e: unknown) => {
        if (live) error = e instanceof Error ? e.message : String(e)
      })
    return () => {
      live = false
    }
  })
</script>

<div class="saved">
  {#if error}
    <p class="crit">could not read it: {error}</p>
  {:else if !saved}
    <p>reading the saved conversation…</p>
  {:else}
    <p class="files">
      <a href={savedContextExportHref(sessionId, index)} download>Export</a> · conversation
      {formatBytes(saved.manifest.conversation_bytes)}{#each saved.files as file (file.path)}
        · {file.path} {formatBytes(file.bytes)}{/each}
    </p>
    <pre>{saved.conversation}</pre>
  {/if}
</div>

<style>
  .saved p {
    margin: 0 0 6px;
    overflow-wrap: anywhere;
  }
  .saved .crit {
    color: var(--crit);
  }
  .saved a {
    color: var(--ink2);
  }
  .saved pre {
    margin: 0;
    max-height: 60vh;
    overflow: auto;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font: inherit;
  }
</style>
