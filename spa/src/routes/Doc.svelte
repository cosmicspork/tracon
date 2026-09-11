<script lang="ts">
  import HtmlImport from '../components/HtmlImport.svelte'
  import { api, ApiError, DocConflict } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { render } from '../lib/markdown'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'
  import type { Document } from '../lib/types'

  let { channel, slug, edit = false }: { channel: string; slug: string; edit?: boolean } = $props()

  let doc = $state<Document | null>(null)
  let missing = $state(false)
  let editing = $state(false)
  let draft = $state('')
  let hash = $state<string | undefined>(undefined)
  let busy = $state(false)
  let error = $state<string | null>(null)
  let loadError = $state<string | null>(null)
  let conflict = $state<{ hash: string; body: string } | null>(null)
  let changedElsewhere = $state(false)
  let loaded = $state(false)
  let previewUrl = $state<string | null>(null)
  let previewError = $state<string | null>(null)
  let replacing = $state(false)

  async function load(keepDraft = false) {
    loadError = null
    try {
      const d = await api.doc(channel, slug)
      doc = d
      missing = false
      if (!keepDraft) {
        draft = d.body
        hash = d.hash
      }
      previewUrl = null
      previewError = null
      replacing = false
      if (d.format === 'html') {
        editing = false
        await mintPreview()
      }
    } catch (e) {
      doc = null
      if (e instanceof ApiError && e.status === 404) {
        missing = true
        if (!keepDraft) {
          draft = `# ${slug.replace(/^[a-z]+-/, '').replace(/-/g, ' ')}\n\n`
          hash = undefined
        }
      } else {
        missing = false
        editing = false
        loadError = e instanceof Error ? e.message : String(e)
      }
    } finally {
      loaded = true
    }
  }

  $effect(() => {
    void channel
    void slug
    loaded = false
    editing = edit
    conflict = null
    changedElsewhere = false
    void load()
  })

  // A change from elsewhere while editing: say so, do not clobber the draft.
  let seenVersion = store.docsVersion
  $effect(() => {
    const v = store.docsVersion
    if (v === seenVersion) return
    seenVersion = v
    if (editing) changedElsewhere = true
    else void load()
  })

  const html = $derived(doc?.format === 'markdown' ? render(doc.body) : '')

  async function save() {
    busy = true
    error = null
    try {
      const d = await api.putDoc(channel, slug, draft, hash)
      doc = d
      hash = d.hash
      missing = false
      editing = false
      conflict = null
      changedElsewhere = false
      if (edit) router.go(`/docs/${channel}/${slug}`)
    } catch (e) {
      if (e instanceof DocConflict) {
        conflict = { hash: e.hash, body: e.body }
      } else {
        error = e instanceof Error ? e.message : String(e)
      }
    } finally {
      busy = false
    }
  }

  function takeTheirs() {
    if (!conflict) return
    draft = conflict.body
    hash = conflict.hash
    conflict = null
    changedElsewhere = false
  }

  function keepMine() {
    if (!conflict) return
    hash = conflict.hash
    conflict = null
    changedElsewhere = false
  }

  async function setArchived(archived: boolean) {
    if (!doc) return
    busy = true
    error = null
    try {
      const d =
        doc.format === 'html'
          ? await api.archiveDoc(channel, slug, archived)
          : await api.putDoc(channel, slug, doc.body, doc.hash, archived)
      doc = d
      hash = d.hash
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }

  async function mintPreview() {
    previewUrl = null
    previewError = null
    try {
      const preview = await api.previewDoc(channel, slug)
      previewUrl = preview.url
    } catch (cause) {
      previewError = cause instanceof Error ? cause.message : String(cause)
    }
  }

  async function downloadOriginal() {
    error = null
    try {
      const { blob, filename } = await api.downloadDoc(channel, slug)
      const href = URL.createObjectURL(blob)
      const link = document.createElement('a')
      link.href = href
      link.download = filename
      link.click()
      URL.revokeObjectURL(href)
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }

  function bundleReplaced(document: Document) {
    doc = document
    hash = document.hash
    replacing = false
    void mintPreview()
  }

  async function remove() {
    if (!doc) return
    busy = true
    try {
      await api.deleteDoc(channel, slug)
      router.go('/docs')
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

{#if !loaded}
  <div class="empty">Loading…</div>
{:else}
  <div class="h4">
    <a class="lnk" href="/docs">Documents</a>
    <span class="sep">/</span>
    {slug}
    <b>{channel}{doc ? ` · ${formatAge(doc.updated_ms, clock.now)}` : ' · new'}{doc ? ` · ${doc.hash.slice(0, 8)}` : ''}{doc?.archived ? ' · archived' : ''}{error && !editing ? ` · ${error}` : ''}</b>
    {#if doc?.format === 'html' && !loadError}
      <span class="r">
        <button class="lnk" onclick={() => (replacing = !replacing)}>{replacing ? 'Cancel replace' : 'Replace bundle'}</button>
        <button class="lnk" onclick={() => setArchived(!doc?.archived)} disabled={busy}>
          {doc.archived ? 'Unarchive' : 'Archive'}
        </button>
        <button class="lnk d" onclick={remove} disabled={busy}>Delete</button>
      </span>
    {:else if !surface.phone && !editing && !loadError}
      <span class="r">
        <button class="lnk" onclick={() => (editing = true)}>{doc ? 'Edit' : 'Write it'}</button>
        {#if doc}
          <button class="lnk" onclick={() => setArchived(!doc?.archived)} disabled={busy}>
            {doc.archived ? 'Unarchive' : 'Archive'}
          </button>
          <button class="lnk d" onclick={remove} disabled={busy}>Delete</button>
        {/if}
      </span>
    {/if}
  </div>

  {#if editing}
    {#if changedElsewhere && !conflict}
      <div class="banner dim">this document changed elsewhere while you were editing <b>· saving will show you the other version first</b></div>
    {/if}
    {#if conflict}
      <div class="banner">changed elsewhere since you read it
        <b>· <button class="lnk" onclick={takeTheirs}>take theirs</button> (drops your draft) or <button class="lnk d" onclick={keepMine}>keep mine</button> (overwrites theirs)</b>
      </div>
    {/if}
    <textarea bind:value={draft} spellcheck="false"></textarea>
    <div class="send">
      <button class="btn p" onclick={save} disabled={busy || loadError !== null || draft === (doc?.body ?? '')}>Save</button>
      <button class="lnk" onclick={() => { editing = false; draft = doc?.body ?? draft; if (!doc) router.go('/docs') }}>Cancel</button>
      {#if error}<span class="err">{error}</span>{/if}
    </div>
  {:else if missing}
    <div class="empty">No document <code>{slug}</code> on {channel}.{#if !surface.phone} <button class="lnk" onclick={() => (editing = true)}>Write it.</button>{/if}</div>
  {:else if loadError}
    <div class="empty err">Could not load this document: {loadError}</div>
  {:else if doc?.format === 'html'}
    <section class="html-doc">
      <div class="html-meta">
        <span><b>Source</b> {doc.source_name}</span>
        <span><b>Entry</b> <code>{doc.entry_path}</code></span>
        <span><b>Generation</b> <code>{doc.hash}</code></span>
        <span><b>Files</b> {doc.bundle_files?.length ?? 0}</span>
      </div>
      <div class="html-actions">
        <a class="btn p" href="/docs/{channel}/{slug}/preview">Full window</a>
        <button class="btn" onclick={downloadOriginal}>Download original</button>
        <button class="btn" onclick={() => (replacing = !replacing)}>
          {replacing ? 'Cancel replace' : 'Replace bundle'}
        </button>
      </div>
      {#if replacing}
        <HtmlImport {channel} initialSlug={slug} ifMatch={doc.hash} onimported={bundleReplaced} />
      {/if}
      {#if previewError}
        <div class="empty err">Could not load the isolated preview: {previewError}</div>
      {:else if previewUrl}
        <iframe title={doc.title} src={previewUrl} sandbox="allow-scripts"></iframe>
      {/if}
      {#if doc.bundle_files?.length}
        <details>
          <summary>Bundle files</summary>
          <div class="bundle-files">
            {#each doc.bundle_files as file (file.path)}
              <code>{file.path}</code><span>{file.media_type} · {file.size_bytes.toLocaleString()} bytes</span>
            {/each}
          </div>
        </details>
      {/if}
      {#if error}<div class="err">{error}</div>{/if}
    </section>
  {:else}
    <article class="md">{@html html}</article>
  {/if}
{/if}

<style>
  .h4 .sep {
    color: var(--dim);
    margin: 0 4px;
  }
  .h4 .r {
    margin-left: auto;
    display: flex;
    gap: 14px;
    letter-spacing: 0;
    text-transform: none;
  }
  textarea {
    width: 100%;
    min-height: 60vh;
    font: 13px/1.5 var(--mono);
    background: var(--s1);
    color: var(--ink);
    border: 0;
    border-radius: 4px;
    padding: 12px 14px;
    resize: vertical;
    box-sizing: border-box;
  }
  .send {
    display: flex;
    gap: 14px;
    align-items: center;
    margin-top: 10px;
  }
  .err {
    color: var(--crit);
    font: 12.5px var(--mono);
  }
  .banner .lnk {
    font: inherit;
  }
  .html-doc { display: grid; gap: 1rem; }
  .html-meta { display: flex; gap: .5rem 1.25rem; flex-wrap: wrap; color: var(--ink2); font-size: .86rem; }
  .html-meta span { min-width: 0; }
  .html-meta code { overflow-wrap: anywhere; }
  .html-actions { display: flex; gap: .5rem; flex-wrap: wrap; }
  .html-actions a { text-decoration: none; }
  iframe { width: 100%; height: min(72vh, 900px); border: 1px solid var(--rule); border-radius: 6px; background: white; }
  details { color: var(--ink2); }
  .bundle-files { display: grid; grid-template-columns: minmax(10rem, 1fr) auto; gap: .35rem 1rem; margin-top: .75rem; font-size: .8rem; }
  .bundle-files span { text-align: right; }
  @media (max-width: 640px) {
    .bundle-files { grid-template-columns: 1fr; }
    .bundle-files span { text-align: left; margin-bottom: .35rem; }
  }
  .md {
    max-width: 72ch;
    line-height: 1.55;
    color: var(--ink);
  }
  .md :global(h1),
  .md :global(h2),
  .md :global(h3) {
    font-weight: 600;
    line-height: 1.25;
    margin: 1.4em 0 0.5em;
    text-wrap: balance;
  }
  .md :global(h1) {
    font-size: 22px;
    margin-top: 0.4em;
  }
  .md :global(h2) {
    font-size: 17px;
  }
  .md :global(h3) {
    font-size: 14.5px;
  }
  .md :global(p),
  .md :global(ul),
  .md :global(ol),
  .md :global(blockquote),
  .md :global(pre),
  .md :global(table) {
    margin: 0 0 0.9em;
  }
  .md :global(li) {
    margin: 0.2em 0;
  }
  .md :global(code) {
    font: 12.5px var(--mono);
    background: var(--s1);
    border-radius: 3px;
    padding: 1px 4px;
  }
  .md :global(pre) {
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 12px;
    overflow-x: auto;
  }
  .md :global(pre code) {
    background: none;
    padding: 0;
  }
  .md :global(a) {
    color: var(--acc);
  }
  .md :global(blockquote) {
    border-left: 3px solid var(--s3);
    padding-left: 12px;
    color: var(--ink2);
  }
  .md :global(table) {
    border-collapse: collapse;
    font-size: 13px;
    display: block;
    overflow-x: auto;
  }
  .md :global(th),
  .md :global(td) {
    border-bottom: 1px solid var(--s2);
    padding: 5px 10px 5px 0;
    text-align: left;
    vertical-align: top;
  }
  .md :global(hr) {
    border: 0;
    border-top: 1px solid var(--s2);
    margin: 1.4em 0;
  }
</style>
