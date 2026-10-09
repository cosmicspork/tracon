<script lang="ts">
  import Card from './Card.svelte'
  import { api } from '../../lib/api'
  import { store } from '../../lib/store.svelte'
  import { acceptsUploads, blankForm, buildLine, canonical, problems, toEntry, toForm, words, type RepoForm } from '../../lib/repos'
  import type { RepoEnvironments, RepoImageBuild } from '../../lib/types'

  const local = $derived(store.node?.loopback ?? false)

  let loaded = $state<RepoEnvironments | null>(null)
  let forms = $state<RepoForm[]>([])
  let busy = $state('')
  let error = $state('')
  let note = $state('')

  const saved = $derived(canonical(loaded?.entries.map((item) => item.entry) ?? []))
  const dirty = $derived(JSON.stringify(forms.map(toEntry)) !== saved)
  const invalid = $derived(problems(forms))
  const presets = $derived(loaded?.presets ?? [])
  const building = $derived(
    loaded?.entries.some((item) => item.builds.some((build) => build.status === 'building')) ?? false,
  )

  // The form is seeded from the node once, and again only after a save: a
  // poll for build progress must never overwrite what is being typed.
  async function load(reseed: boolean) {
    try {
      loaded = await api.repoEnvironments()
      if (reseed) forms = loaded.entries.map((item) => toForm(item.entry))
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }
  void load(true)

  $effect(() => {
    if (!building) return
    const timer = setInterval(() => void load(false), 2000)
    return () => clearInterval(timer)
  })

  function builds(path: string): RepoImageBuild[] {
    return loaded?.entries.find((item) => item.entry.path === path)?.builds ?? []
  }

  async function save() {
    if (busy || invalid.length) return
    busy = 'save'
    note = ''
    try {
      await api.saveRepoEnvironments(forms.map(toEntry))
      await load(true)
      note = 'Saved to node.toml. It applies to the next session and the next check; no restart.'
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }

  async function build(repo: string) {
    if (busy) return
    busy = `build:${repo}`
    note = ''
    try {
      await api.buildRepoEnvironment(repo)
      // The build's row appears a moment after the request is accepted.
      await new Promise((resolve) => setTimeout(resolve, 700))
      await load(false)
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }

  function add() {
    forms = [...forms, blankForm()]
  }

  function remove(index: number) {
    forms = forms.filter((_, at) => at !== index)
  }
</script>

<Card
  title="Repository environments"
  note="What the node does for one repository: the image its sessions and checks run in, the checks themselves, and how its dependencies get there. These are yours and live in node.toml, never in the repository — a candidate that could edit them could choose its own checks."
>
  {#if !local}
    <p class="blocked">Repository environments are changed on the node itself.</p>
  {/if}

  {#if loaded}
    {#if forms.length === 0}
      <p class="dim">No repository has an environment of its own: every one runs in the harness image, with the node-wide checks and no preparation.</p>
    {/if}
    {#each forms as form, index (index)}
      {@const built = builds(form.path)}
      {@const uploads = acceptsUploads(words(form.egress), presets)}
      <details class="entry" open={!form.path || forms.length <= 2}>
        <summary>
          <code>{form.path || 'new repository'}</code>
          <small>
            {form.source === 'dockerfile' ? `builds ${form.dockerfile || '…'}` : form.source === 'image' ? 'pinned image' : 'harness image'}{form.sessionEgress ? ' · registries open to sessions' : ''}
          </small>
        </summary>
        <div class="grid">
          <label class="wide">
            <span>Repository</span>
            <input bind:value={form.path} disabled={!local} spellcheck="false" placeholder="github.com/owner/name, or /an/absolute/checkout" />
            <small>An absolute path is that repository's root. A relative one matches the end of a path, so a clone the node manages is named without the clone root. The first matching entry wins.</small>
          </label>

          <label>
            <span>Image</span>
            <select bind:value={form.source} disabled={!local}>
              <option value="dockerfile">Built from its Dockerfile</option>
              <option value="image">Pinned by hand</option>
              <option value="harness">The harness image</option>
            </select>
            <small>
              {#if form.source === 'dockerfile'}Built from the default branch, never from a candidate. Only the Dockerfile is honoured; a devcontainer's hooks and services are not.
              {:else if form.source === 'image'}Digest-pinned. You build it and keep it current.
              {:else}No project toolchain: a check whose command is missing is reported as not runnable.{/if}
            </small>
          </label>
          {#if form.source === 'dockerfile'}
            <label>
              <span>Dockerfile</span>
              <input bind:value={form.dockerfile} disabled={!local} spellcheck="false" placeholder=".devcontainer/Dockerfile" />
            </label>
            <label>
              <span>Build context</span>
              <input bind:value={form.context} disabled={!local} spellcheck="false" placeholder="the Dockerfile's directory" />
            </label>
          {:else if form.source === 'image'}
            <label class="wide">
              <span>Image</span>
              <input bind:value={form.image} disabled={!local} spellcheck="false" placeholder="localhost/toolchain@sha256:…" />
            </label>
          {/if}

          <div class="wide sessions">
            <span class="heading">Required checks</span>
            <label class="toggle"><input type="checkbox" bind:checked={form.ownChecks} disabled={!local} /> This repository has its own</label>
            {#if form.ownChecks}
              <textarea bind:value={form.checks} disabled={!local} rows="2" spellcheck="false" placeholder="just check" aria-label="Required checks, one per line"></textarea>
              <small>One command per line. Left empty, this repository has no required checks.</small>
            {:else}
              <small>The node-wide checks run.</small>
            {/if}
          </div>

          <label class="wide">
            <span>Preparation</span>
            <textarea bind:value={form.prepare} disabled={!local} rows="2" spellcheck="false" placeholder="cargo fetch --locked"></textarea>
            <small>One command per line, run once before a run's checks with the registries below reachable. The checks that follow have no network.</small>
          </label>

          <label>
            <span>Registries</span>
            <input bind:value={form.egress} disabled={!local} spellcheck="false" placeholder={presets.map(([name]) => name).join(' ')} />
            <small>A preset ({presets.map(([name]) => name).join(', ')}) or a host name. Empty reaches nothing.</small>
          </label>
          <label>
            <span>Time limit (seconds)</span>
            <input type="number" min="1" bind:value={form.timeout} disabled={!local} placeholder="node-wide" />
            <small>For preparation, and for each check.</small>
          </label>

          <div class="wide sessions">
            <label class="toggle"><input type="checkbox" bind:checked={form.sessionEgress} disabled={!local} /> Open these registries to this repository's sessions</label>
            <small>So an agent can add a dependency and run what it installed. A session is long-lived and runs what a model decides: anything it can reach that accepts uploads, it can upload to.</small>
            {#if form.sessionEgress && uploads.length}
              <p class="caution">{uploads.join(', ')} reach{uploads.length === 1 ? 'es' : ''} GitHub, which accepts uploads from anyone holding a token.</p>
            {/if}
          </div>
        </div>

        {#if built.length}
          <ul class="builds">
            {#each built as item (item.id)}
              <li class:failed={item.status === 'failed'}>
                <span>{item.repo_path} · {buildLine(item)}</span>
                {#each item.warnings as command (command)}
                  <small class="caution">The image has no tool for <code>{command}</code>.</small>
                {/each}
                {#if item.status === 'failed' && item.log_tail}
                  <details><summary>Build output</summary><pre>{item.log_tail}</pre></details>
                {/if}
                {#if item.kind === 'base' && loaded.can_build}
                  <button class="lnk" type="button" onclick={() => void build(item.repo_path)} disabled={!local || busy !== '' || item.status === 'building'}>
                    {busy === `build:${item.repo_path}` ? 'Starting…' : 'Build again'}
                  </button>
                {/if}
              </li>
            {/each}
          </ul>
        {:else if form.source === 'dockerfile' && form.path}
          <p class="dim">
            Not built yet: the first session or check on it builds it, and waits.
            {#if loaded.can_build}
              <button class="lnk" type="button" onclick={() => void build(form.path)} disabled={!local || busy !== '' || dirty}>
                {busy === `build:${form.path}` ? 'Starting…' : 'Build now'}
              </button>{#if dirty}<small> Save first.</small>{/if}
            {:else}
              This runtime cannot build images.
            {/if}
          </p>
        {/if}

        <div class="acts">
          <button class="lnk d" type="button" onclick={() => remove(index)} disabled={!local || busy !== ''}>Remove this entry</button>
        </div>
      </details>
    {/each}

    <div class="acts">
      <button class="lnk" type="button" onclick={add} disabled={!local || busy !== ''}>+ Add a repository</button>
      {#if dirty}
        <button class="btn p" type="button" onclick={() => void save()} disabled={!local || busy !== '' || invalid.length > 0}>
          {busy === 'save' ? 'Saving…' : 'Save repositories'}
        </button>
      {/if}
      {#if dirty && invalid.length}<small class="bad">{invalid[0]}</small>{/if}
    </div>
  {:else if !error}
    <p class="dim">Reading the repository table…</p>
  {/if}

  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if note}<p class="note" role="status">{note}</p>{/if}
</Card>

<style>
  .entry { background: var(--s2); border-radius: 4px; padding: 10px 12px; display: grid; gap: 12px; }
  .entry > summary { cursor: pointer; display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 12px; }
  .entry > summary code { font: 13px var(--mono); overflow-wrap: anywhere; }
  .entry[open] > summary { margin-bottom: 12px; }
  .grid { display: grid; gap: 14px 16px; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); align-items: start; }
  .grid .wide { grid-column: 1 / -1; }
  label, .sessions { display: grid; gap: 5px; align-content: start; min-width: 0; }
  label > span:first-child, .heading { font: 500 11px var(--mono); letter-spacing: 0.08em; text-transform: uppercase; color: var(--ink2); }
  input:not([type='checkbox']), select, textarea {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--mono);
    min-width: 0;
    min-height: 36px;
  }
  select { font-family: var(--sans); }
  textarea { resize: vertical; }
  input:disabled, select:disabled, textarea:disabled { opacity: 0.5; }
  label.toggle { display: flex; align-items: center; gap: 6px; color: var(--ink); }
  small { font-size: 12.5px; color: var(--dim); }
  .builds { margin: 12px 0 0; padding: 0; list-style: none; display: grid; gap: 4px; }
  .builds li { display: grid; gap: 3px; justify-items: start; background: var(--s1); border-radius: 4px; padding: 6px 10px; font: 12px var(--mono); overflow-wrap: anywhere; }
  .builds li.failed > span { color: var(--crit); }
  .builds pre { margin: 6px 0 0; max-height: 240px; overflow: auto; white-space: pre-wrap; color: var(--ink2); }
  .builds details summary { cursor: pointer; color: var(--ink2); }
  .acts { display: flex; flex-wrap: wrap; align-items: center; gap: 8px 14px; }
  .entry .acts { margin-top: 12px; }
  .caution { margin: 0; color: var(--wait); font-size: 12.5px; }
  .bad, .error, .blocked { color: var(--crit); }
  .dim, .note, .error { margin: 0; font-size: 12.5px; }
  .dim { color: var(--dim); }
  .note { color: var(--ok); }
  .blocked { margin: 0; background: var(--wash-crit); border-left: 3px solid var(--crit); padding: 8px 10px; }
</style>
