<script lang="ts">
  // What a channel's sessions launch with, beyond what the node decides for
  // them: the operator's standing notes, skills, agents, and the plugins and
  // language servers the harness image made available.
  //
  // The pane shows two things on purpose. The *selection* is what is imported
  // now; the *manifest* is the revision and digest a launch would build from
  // it. They are not the same fact — a running session holds the revision it
  // staged, and nothing here reaches it — so saying "applies at the next
  // launch" is the honest thing to put in front of an operator.
  //
  // Notes are edited here rather than only over the API because they are the
  // one part of a session's orientation the node does not write for itself:
  // everything else it is told is tracon's account of the installation, and a
  // standing note is the operator's own. Something that reaches every session
  // on a channel needs a place the operator can find it and read it back.
  import { api } from '../lib/api'
  import { store } from '../lib/store.svelte'
  import type { ManifestView } from '../lib/types'
  import Card from './settings/Card.svelte'

  const INSTRUCTION = 'instruction'

  let channel = $state('')
  let view = $state<ManifestView | null>(null)
  let source = $state('')
  let noteName = $state('')
  let noteBody = $state('')
  let busy = $state('')
  let error = $state('')
  let note = $state('')
  let warnings = $state<string[]>([])

  const local = $derived(store.node?.loopback ?? false)
  const channels = $derived(store.channels.filter((c) => !c.archived))
  const current = $derived(channel || channels[0]?.name || 'personal')

  async function load(name: string) {
    try {
      view = await api.manifest(name)
      error = ''
    } catch (e) {
      view = null
      error = e instanceof Error ? e.message : String(e)
    }
  }

  // One read per channel the operator selects, not an effect on every keystroke.
  $effect(() => {
    void load(current)
  })

  async function act(what: string, f: () => Promise<unknown>) {
    busy = what
    error = ''
    note = ''
    try {
      await f()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = ''
      await load(current)
    }
  }

  const importSkill = () =>
    act('import', async () => {
      const result = await api.importSkill(current, source.trim())
      warnings = result.warnings
      note = `${result.name} imported — ${result.files.length} file(s). Applies at the next launch.`
      source = ''
    })

  const remove = (kind: string, name: string) =>
    act('remove', async () => {
      await api.removeManifestItem(current, kind, name)
      warnings = []
      note = `${name} removed. Running sessions keep the manifest they launched with.`
    })

  const notes = $derived(view?.items.filter((i) => i.kind === INSTRUCTION) ?? [])
  const installed = $derived(view?.items.filter((i) => i.kind !== INSTRUCTION) ?? [])

  // Saving under an existing name replaces that note, which is also how one is
  // edited: the store keys a manifest item by (channel, kind, name).
  const saveNote = () =>
    act('note', async () => {
      const name = noteName.trim()
      await api.putManifestText(current, INSTRUCTION, name, noteBody)
      note = `${name} saved. Sessions already running keep the notes they launched with.`
      noteName = ''
      noteBody = ''
    })

  function editNote(name: string, body: string) {
    noteName = name
    noteBody = body
  }
</script>

<Card title="Customization" note="The operator notes, skills and agents a channel's sessions launch with. A change applies at the next launch, never to a running session.">

  <label class="pick">
    <span>Channel</span>
    <select value={current} onchange={(event) => (channel = event.currentTarget.value)}>
      {#each channels as c (c.name)}<option value={c.name}>{c.name}</option>{/each}
    </select>
  </label>

  {#if view}
    <div class="rev">
      {#if view.next}
        <span class="chip">next launch r{view.next.revision}</span>
        <span class="mono">{view.next.digest.slice(0, 12)}</span>
      {:else}
        <span class="chip bad">will not build</span>
      {/if}
      {#if view.recorded}
        <span class="mono dim">last recorded r{view.recorded.revision}</span>
      {/if}
    </div>

    {#if view.error}
      <p class="why"><b>This manifest would be refused at launch</b><i>{view.error}</i></p>
    {/if}

    {#if installed.length}
      <div class="items">
        {#each installed as item (item.kind + item.name)}
          <div class="item">
            <span class="chip">{item.kind}</span>
            <span class="name">{item.name}</span>
            <span class="mono dim">{item.source}</span>
            {#if item.digest}<span class="mono dim">{item.digest.slice(0, 8)}</span>{/if}
            <button
              class="btn"
              disabled={!local || busy !== ''}
              onclick={() => remove(item.kind, item.name)}>Remove</button
            >
            {#each item.warnings as w (w)}<small class="warn">{w}</small>{/each}
          </div>
        {/each}
      </div>
    {:else}
      <div class="empty">
        No skills or agents imported. Sessions here launch with the node's orientation and
        whatever notes are below.
      </div>
    {/if}

    <div class="notes">
      <h4>Operator notes</h4>
      <small>
        Standing notes every session on this channel reads. They are their own section of a
        session's orientation, kept apart from what the node says about itself: tracon's half is
        the node, the work, the tools and what is refused; this half is yours. Saving under an
        existing name replaces that note.
      </small>
      {#each notes as n (n.name)}
        <div class="note-row">
          <span class="name">{n.name}</span>
          <button class="btn" disabled={!local || busy !== ''} onclick={() => editNote(n.name, n.body)}
            >Edit</button
          >
          <button class="btn" disabled={!local || busy !== ''} onclick={() => remove(n.kind, n.name)}
            >Remove</button
          >
          <p class="body">{n.body}</p>
        </div>
      {/each}
      <label>
        <span>Name</span>
        <input bind:value={noteName} disabled={!local} spellcheck="false" placeholder="house-style" />
      </label>
      <label>
        <span>Note</span>
        <textarea
          bind:value={noteBody}
          disabled={!local}
          rows="4"
          placeholder="Prefer small commits. Ask before adding a dependency."
        ></textarea>
      </label>
      <div class="acts">
        <button
          class="btn p"
          disabled={!local || busy !== '' || noteName.trim() === '' || noteBody.trim() === ''}
          onclick={saveNote}>{busy === 'note' ? 'Saving…' : 'Save note'}</button
        >
      </div>
    </div>

    <label>
      <span>Import a skill</span>
      <input
        bind:value={source}
        disabled={!local}
        spellcheck="false"
        placeholder="/path/to/skill-package, or /path#git-rev"
      />
      <small>
        A directory on this machine holding a SKILL.md. A URL is refused: the node stages bytes it
        has read, never a fetch the harness makes. Skill content is code — its scripts run in the
        runner and its body is also a slash command.
      </small>
    </label>
    <div class="acts">
      <button
        class="btn p"
        disabled={!local || busy !== '' || source.trim() === ''}
        onclick={importSkill}>{busy === 'import' ? 'Importing…' : 'Import'}</button
      >
      {#if note}<small>{note}</small>{/if}
    </div>
    {#each warnings as w (w)}<small class="warn">{w}</small>{/each}

    <div class="image">
      <span class="dim">From the harness image, and not editable here:</span>
      <span class="mono"
        >plugins {view.baked_plugins.length ? view.baked_plugins.join(', ') : 'none baked'}</span
      >
      {#if view.next}
        <span class="mono"
          >lsp {view.next.lsp.length ? view.next.lsp.map((l) => l.name).join(', ') : 'none'}</span
        >
        <span class="mono"
          >formatters {view.next.formatters.length
            ? view.next.formatters.map((f) => f.name).join(', ')
            : 'none'}</span
        >
      {/if}
    </div>
  {:else if error}
    <p class="why"><b>The manifest could not be read</b><i>{error}</i></p>
  {:else}
    <div class="empty">Reading the manifest…</div>
  {/if}
</Card>

<style>
  label {
    display: grid;
    gap: 5px;
    min-width: 0;
    max-width: 560px;
  }
  .pick {
    max-width: 320px;
  }
  label > span {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  input,
  select,
  textarea {
    background: var(--s2);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
    min-width: 0;
  }
  textarea {
    resize: vertical;
  }
  .notes {
    display: grid;
    gap: 8px;
    max-width: 560px;
    min-width: 0;
  }
  .notes h4 {
    margin: 0;
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  .note-row {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
    padding: 6px 0;
    border-top: 1px solid var(--rule);
  }
  .note-row .body {
    flex-basis: 100%;
    margin: 0;
    white-space: pre-wrap;
    font: 12.5px var(--sans);
    color: var(--dim);
  }
  .rev,
  .image,
  .acts {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .items {
    display: grid;
    gap: 6px;
  }
  .item {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
    padding: 6px 0;
    border-top: 1px solid var(--rule);
  }
  .name {
    font: 500 13px var(--sans);
    color: var(--ink);
  }
  .mono {
    font: 12px var(--mono);
    color: var(--ink2);
  }
  small {
    font-size: 12.5px;
    color: var(--dim);
  }
  .warn {
    flex-basis: 100%;
    color: var(--wait);
  }
  .dim {
    color: var(--dim);
    font-size: 12.5px;
  }
  .why {
    margin: 0;
    font: 12.5px var(--mono);
    color: var(--crit);
    display: grid;
    gap: 4px;
  }
</style>
