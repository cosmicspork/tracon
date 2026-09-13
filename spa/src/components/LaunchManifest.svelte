<script lang="ts">
  // What a channel's sessions launch with, beyond what the node decides for
  // them: skills, standing instructions, agents, and the plugins and language
  // servers the harness image made available.
  //
  // The pane shows two things on purpose. The *selection* is what is imported
  // now; the *manifest* is the revision and digest a launch would build from
  // it. They are not the same fact — a running session holds the revision it
  // staged, and nothing here reaches it — so saying "applies at the next
  // launch" is the honest thing to put in front of an operator.
  import { api } from '../lib/api'
  import { store } from '../lib/store.svelte'
  import type { ManifestView } from '../lib/types'

  let channel = $state('')
  let view = $state<ManifestView | null>(null)
  let source = $state('')
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
</script>

<section>
  <div class="h5">
    Customization <b>skills, instructions and agents this channel's sessions launch with</b>
  </div>

  <label class="pick">
    <span>Channel</span>
    <select bind:value={channel}>
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
      <span class="dim">A new revision never changes a running session.</span>
    </div>

    {#if view.error}
      <p class="why"><b>This manifest would be refused at launch</b><i>{view.error}</i></p>
    {/if}

    {#if view.items.length}
      <div class="items">
        {#each view.items as item (item.kind + item.name)}
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
      <div class="empty">Nothing imported. Sessions here launch with the node's orientation only.</div>
    {/if}

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
</section>

<style>
  section {
    display: grid;
    gap: 8px;
    background: var(--s1);
    border-radius: 4px;
    padding: 14px 16px;
    max-width: 720px;
  }
  .h5 {
    font: 500 13px var(--sans);
    color: var(--ink);
  }
  .h5 b {
    font-weight: 400;
    color: var(--dim);
    margin-left: 6px;
  }
  label,
  .pick {
    display: grid;
    gap: 5px;
    min-width: 0;
  }
  label > span {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  input,
  select {
    background: var(--s2);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
    min-width: 0;
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
