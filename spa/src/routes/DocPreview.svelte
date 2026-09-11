<script lang="ts">
  import { api } from '../lib/api'

  let { channel, slug }: { channel: string; slug: string } = $props()
  let previewUrl = $state<string | null>(null)
  let error = $state<string | null>(null)

  $effect(() => {
    void channel
    void slug
    previewUrl = null
    error = null
    api
      .previewDoc(channel, slug)
      .then((preview) => (previewUrl = preview.url))
      .catch((cause) => (error = cause instanceof Error ? cause.message : String(cause)))
  })
</script>

<div class="preview-window">
  <header>
    <a href="/docs/{channel}/{slug}">Back to document</a>
    <span>{slug}</span>
  </header>
  {#if error}
    <div class="failure">Could not load the isolated preview: {error}</div>
  {:else if previewUrl}
    <iframe title={slug} src={previewUrl} sandbox="allow-scripts"></iframe>
  {:else}
    <div class="failure">Loading preview…</div>
  {/if}
</div>

<style>
  .preview-window { min-height: 100dvh; display: grid; grid-template-rows: auto 1fr; background: var(--bg); }
  header { min-width: 0; display: flex; align-items: center; gap: 1rem; padding: .65rem 1rem; border-bottom: 1px solid var(--rule); background: var(--s1); }
  header a { font: 12px var(--mono); text-decoration: none; }
  header span { color: var(--ink2); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  iframe { width: 100%; height: 100%; min-height: 0; border: 0; background: white; }
  .failure { padding: 2rem; color: var(--crit); }
</style>
