<script lang="ts">
  import { onMount } from 'svelte'
  import { api, DocConflict } from '../lib/api'
  import { selectHtmlFile, selectHtmlFolder, type HtmlBundleSelection } from '../lib/html-bundle'
  import type { Document } from '../lib/types'

  let {
    channel,
    initialSlug = '',
    ifMatch,
    onimported,
  }: {
    channel: string
    initialSlug?: string
    ifMatch?: string
    onimported: (document: Document) => void
  } = $props()

  let selection = $state<HtmlBundleSelection | null>(null)
  let slug = $state('')
  let error = $state<string | null>(null)
  let status = $state<string | null>(null)
  let uploading = $state(false)
  let folderAvailable = $state(false)

  onMount(() => {
    slug = initialSlug
    folderAvailable = 'webkitdirectory' in document.createElement('input')
  })

  function choose(input: FileList | null, folder: boolean) {
    if (!input) return
    status = 'Validating selection…'
    error = null
    try {
      selection = folder ? selectHtmlFolder(input) : selectHtmlFile(input)
      if (!ifMatch) slug = selection.suggestedSlug
      status = 'Ready to upload'
    } catch (cause) {
      selection = null
      status = null
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }

  async function upload() {
    if (!selection || !channel || !slug.trim()) return
    uploading = true
    error = null
    status = 'Uploading bundle…'
    try {
      const document = await api.importHtml(channel, slug.trim().toLowerCase(), selection, ifMatch)
      status = 'Import complete'
      onimported(document)
    } catch (cause) {
      status = null
      error =
        cause instanceof DocConflict
          ? 'The document changed since this page loaded. Reload it before replacing the bundle.'
          : cause instanceof Error
            ? cause.message
            : String(cause)
    } finally {
      uploading = false
    }
  }
</script>

<div class="html-import">
  <div class="pickers">
    <label class="btn">
      Choose HTML file
      <input
        type="file"
        accept="text/html,.html,.htm"
        onchange={(event) => choose(event.currentTarget.files, false)}
      />
    </label>
    <label class:disabled={!folderAvailable} class="btn">
      Choose folder
      <input
        type="file"
        webkitdirectory
        multiple
        disabled={!folderAvailable}
        onchange={(event) => choose(event.currentTarget.files, true)}
      />
    </label>
  </div>
  {#if !folderAvailable}
    <small>Folder import is not available in this browser. Single-file import still works.</small>
  {/if}

  {#if selection}
    <div class="summary">
      <span><b>Entry</b> <code>{selection.entryPath}</code></span>
      <span><b>Files</b> {selection.files.length}</span>
      <span><b>Size</b> {selection.totalBytes.toLocaleString()} bytes</span>
      <span><b>Channel</b> {channel}</span>
    </div>
    <label class="slug">
      Slug
      <input bind:value={slug} disabled={Boolean(ifMatch)} placeholder="ref-document" />
    </label>
    <button class="btn p" onclick={upload} disabled={uploading || !slug.trim()}>
      {uploading ? 'Uploading…' : ifMatch ? 'Replace bundle' : 'Import HTML'}
    </button>
  {/if}
  {#if status}<small class="status">{status}</small>{/if}
  {#if error}<div class="error">{error}</div>{/if}
</div>

<style>
  .html-import { display: grid; gap: .75rem; padding: 1rem; border: 1px solid var(--rule); border-radius: 10px; background: var(--s1); }
  .pickers { display: flex; gap: .5rem; flex-wrap: wrap; }
  .pickers input[type='file'] { position: absolute; inline-size: 1px; block-size: 1px; opacity: 0; pointer-events: none; }
  .disabled { opacity: .45; cursor: not-allowed; }
  .summary { display: flex; gap: .75rem 1.25rem; flex-wrap: wrap; font-size: .86rem; color: var(--ink2); }
  .summary span { white-space: nowrap; }
  .slug { display: grid; gap: .35rem; font-size: .8rem; color: var(--ink2); }
  .slug input { color: var(--ink); }
  .status { color: var(--ink2); }
  .error { color: var(--crit); font-size: .86rem; }
</style>
