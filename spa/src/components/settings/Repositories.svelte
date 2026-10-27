<script lang="ts">
  import Help from '../Help.svelte'
  import Card from './Card.svelte'
  import RecipeOffers from './RecipeOffers.svelte'
  import { api } from '../../lib/api'
  import { autogrow } from '../../lib/autogrow'
  import { store } from '../../lib/store.svelte'
  import { forgeIdentity, offerResultLine, offerTargets } from '../../lib/recipes'
  import {
    acceptsUploads,
    blankForm,
    buildLine,
    canonical,
    problems,
    REPO_FIELDS,
    repoEdit,
    replayRepos,
    toForm,
    words,
    writtenRepos,
    type RepoForm,
  } from '../../lib/repos'
  import { saveOps } from '../../lib/config-form'
  import type { RecipeOfferResult, RepoEntry, RepoEnvironments, RepoImageBuild } from '../../lib/types'

  let loaded = $state<RepoEnvironments | null>(null)
  let forms = $state<RepoForm[]>([])
  // The table the forms were seeded from, and its file's version: a save
  // edits entries where they were in it and names this version, never a
  // later poll's. Both come from one read of node.toml (`GET /api/config`),
  // so the indices the save names are that version's; the repository list
  // (`/api/repos/environments`) is read only for builds and holds.
  let seeded = $state<RepoEntry[]>([])
  let seededHash = ''
  /** The table as the node has it now, after a save met a newer version. */
  let current: { entries: RepoEntry[]; hash: string } | null = null

  let busy = $state('')
  let error = $state('')
  let note = $state('')
  /** The table changed on the node while it was being edited here. */
  let moved = $state(false)
  let fieldErrors = $state<Record<string, string[]>>({})

  const edit = $derived(repoEdit(seeded, forms))
  const ops = $derived(edit.ops)
  const dirty = $derived(ops.length > 0)
  const invalid = $derived(problems(forms))

  async function seed() {
    const cfg = await api.config()
    seeded = writtenRepos(cfg.document)
    seededHash = cfg.hash
    forms = seeded.map((entry, origin) => ({ ...toForm(entry), origin }))
    moved = false
    current = null
    fieldErrors = {}
  }

  function entryErrors(index: number): string[] {
    return Object.entries(fieldErrors)
      .filter(([key]) => key === `repo[${index}]` || key.startsWith(`repo[${index}].`))
      .flatMap(([, messages]) => messages)
  }
  const presets = $derived(loaded?.presets ?? [])
  const building = $derived(
    loaded?.entries.some((item) => item.builds.some((build) => build.status === 'building')) ?? false,
  )

  // The form is seeded from the node once, and again only after a save: a
  // poll for build progress must never overwrite what is being typed.
  async function load(reseed: boolean) {
    try {
      const [environments] = await Promise.all([api.repoEnvironments(), reseed ? seed() : null])
      loaded = environments
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

  function holdOf(path: string) {
    return loaded?.entries.find((item) => item.entry.path === path)?.hold ?? null
  }

  async function startHeld(path: string, lendingHash: string) {
    if (busy) return
    busy = `start:${path}`
    note = ''
    try {
      const started = await api.startHeldBuild(path, lendingHash)
      note = started.building ? 'Building with the offered preparation now.' : 'Released: the next session or check builds it.'
      await load(false)
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }

  // Each change is its own operation on `[[repo]]`: an entry removed,
  // added or moved, a field of one set or taken out. Off the node's machine
  // the node confirms the save with a passkey that names the keys.
  async function save() {
    if (busy || invalid.length || !ops.length) return
    busy = 'save'
    note = ''
    error = ''
    fieldErrors = {}
    // `repo` itself: an entry being added has no index the node can name.
    const keys = ['repo', ...forms.flatMap((_, i) => [`repo[${i}]`, ...REPO_FIELDS.map((name) => `repo[${i}].${name}`)])]
    const outcome = await saveOps((hash, o) => api.patchConfig(hash, o), seededHash, ops, keys, edit.screen)
    try {
      if (outcome.kind === 'saved') {
        await load(true)
        void store.refreshRestart()
        note = 'Saved to node.toml. It applies to the next session and the next check; no restart.'
      } else if (outcome.kind === 'conflict') {
        await reread()
      } else if (outcome.kind === 'refused') {
        const { repo: added = [], ...placed } = outcome.fields
        fieldErrors = placed
        error = [...added, ...outcome.general].join(' ') || 'Refused: the entries it is about are marked.'
      } else {
        error = outcome.message
      }
    } finally {
      busy = ''
    }
  }

  // node.toml moved under the forms. If the table itself did not, the edits
  // apply as they are to the version now; if it did, the operator decides.
  async function reread() {
    const cfg = await api.config()
    const now = writtenRepos(cfg.document)
    if (canonical(now) === canonical(seeded)) {
      seededHash = cfg.hash
      note = 'node.toml changed elsewhere since this was read; the repository table did not. Save again to apply your edits.'
      return
    }
    current = { entries: now, hash: cfg.hash }
    moved = true
  }

  /** Carry these edits onto the table as the node has it now: only the
      fields changed here are replayed, so nobody else's change is undone,
      an entry the node added stays, and one it removed is named. */
  function keepMine() {
    if (!current) return
    const out = replayRepos(seeded, forms, current.entries)
    seeded = current.entries
    seededHash = current.hash
    forms = out.forms
    current = null
    moved = false
    const said = ['Your edits now apply to the table as the node has it. Review and save.']
    if (out.removed.length) said.push(`Removed on the node, so your edits to them were dropped: ${out.removed.join(', ')}.`)
    if (out.clashes.length) said.push(`Changed on the node too; saving writes yours: ${out.clashes.join('; ')}.`)
    note = said.join(' ')
  }

  function move(from: number, to: number) {
    const next = [...forms]
    const [entry] = next.splice(from, 1)
    next.splice(to, 0, entry)
    forms = next
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

  // Offering an entry's recipe to channel co-members. Only a saved entry is
  // offered: the node reads it from its own table.
  const targets = $derived(offerTargets(store.channels, store.nodes, store.node?.id ?? ''))
  let offering = $state<string | null>(null)
  let offerChannel = $state('')
  let offerNodes = $state<string[]>([])
  let offerRepo = $state('')
  let offerNote = $state('')
  let offerResults = $state<RecipeOfferResult[]>([])
  const offerPeers = $derived(targets.find((target) => target.channel === offerChannel)?.peers ?? [])

  function startOffer(path: string) {
    offering = offering === path ? null : path
    offerChannel = targets[0]?.channel ?? ''
    offerNodes = (targets[0]?.peers ?? []).map((peer) => peer.id)
    offerRepo = forgeIdentity(path) ?? ''
    offerNote = ''
    offerResults = []
  }

  function chooseChannel(channel: string) {
    offerChannel = channel
    offerNodes = (targets.find((target) => target.channel === channel)?.peers ?? []).map((peer) => peer.id)
  }

  async function sendOffer(path: string) {
    if (busy || !offerChannel || !offerNodes.length) return
    busy = `offer:${path}`
    error = ''
    try {
      const sent = await api.offerRecipe({
        path,
        repo: offerRepo.trim() || undefined,
        channel: offerChannel,
        to_nodes: offerNodes,
        note: offerNote.trim() || undefined,
      })
      offerResults = sent.results
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
  note="Image, checks and dependency setup per repository. Kept in node.toml so a candidate can't choose its own checks."
>
  {#if moved}
    <div class="held" role="alert">
      <p>Changed on the node while you were editing.</p>
      <div class="acts">
        <button class="btn" type="button" onclick={keepMine} disabled={busy !== ''}>Keep my edits</button>
        <button class="lnk" type="button" onclick={() => void load(true)} disabled={busy !== ''}>Discard them and reload</button>
      </div>
    </div>
  {/if}

  <RecipeOffers onaccepted={() => void load(true)} />

  {#if loaded}
    {#if forms.length === 0}
      <p class="dim">None. Every repository uses the harness image and node-wide checks.</p>
    {/if}
    {#each forms as form, index (index)}
      {@const built = builds(form.path)}
      {@const hold = holdOf(form.path)}
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
            <input bind:value={form.path} spellcheck="false" placeholder="github.com/owner/name, or /an/absolute/checkout" />
            <small>First match wins. <Help label="About repository matching">An absolute path is a repository root. A relative one matches the end of a path, so managed clones need no clone root.</Help></small>
          </label>

          <label>
            <span>Image</span>
            <select bind:value={form.source} >
              <option value="dockerfile">Built from its Dockerfile</option>
              <option value="image">Pinned by hand</option>
              <option value="harness">The harness image</option>
            </select>
            <small>
              {#if form.source === 'dockerfile'}From the default branch. Devcontainer hooks and services are ignored.
              {:else if form.source === 'image'}Digest-pinned; you keep it current.
              {:else}No project toolchain.{/if}
            </small>
          </label>
          {#if form.source === 'dockerfile'}
            <label>
              <span>Dockerfile</span>
              <input bind:value={form.dockerfile} spellcheck="false" placeholder=".devcontainer/Dockerfile" />
            </label>
            <label>
              <span>Build context</span>
              <input bind:value={form.context} spellcheck="false" placeholder="the Dockerfile's directory" />
            </label>
          {:else if form.source === 'image'}
            <label class="wide">
              <span>Image</span>
              <input bind:value={form.image} spellcheck="false" placeholder="localhost/toolchain@sha256:…" />
            </label>
          {/if}

          <div class="wide sessions">
            <span class="heading">Required checks</span>
            <label class="toggle"><input type="checkbox" bind:checked={form.ownChecks} /> This repository has its own</label>
            {#if form.ownChecks}
              <textarea class="grow" bind:value={form.checks} use:autogrow={form.checks} rows="2" spellcheck="false" placeholder="just check" aria-label="Required checks, one per line"></textarea>
              <small>One per line. Empty: no required checks.</small>
            {:else}
              <small>The node-wide checks run.</small>
            {/if}
          </div>

          <label class="wide">
            <span>Preparation</span>
            <textarea class="grow" bind:value={form.prepare} use:autogrow={form.prepare} rows="2" spellcheck="false" placeholder="cargo fetch --locked"></textarea>
            <small>One per line, before checks, with registries reachable. Checks run offline.</small>
          </label>

          <label>
            <span>Registries</span>
            <input bind:value={form.egress} spellcheck="false" placeholder={presets.map(([name]) => name).join(' ')} />
            <small>Presets: {presets.map(([name]) => name).join(', ')} · or a host</small>
          </label>
          <label>
            <span>Time limit (seconds)</span>
            <input type="number" min="1" bind:value={form.timeout} placeholder="node-wide" />
            <small>For preparation, and for each check.</small>
          </label>

          <div class="wide sessions">
            <label class="toggle"><input type="checkbox" bind:checked={form.sessionEgress} /> Open these registries to this repository's sessions</label>
            <small>Lets an agent install dependencies. <Help label="About session registries">Sessions run what a model decides; anything reachable that accepts uploads can receive them.</Help></small>
            {#if form.sessionEgress && uploads.length}
              <p class="caution">{uploads.join(', ')} reach{uploads.length === 1 ? 'es' : ''} GitHub, which accepts uploads.</p>
            {/if}
          </div>
        </div>

        {#each entryErrors(index) as message (message)}<p class="error" role="alert">{message}</p>{/each}

        {#if hold}
          <div class="held" role="status">
            <p>
              Held: a peer's recipe changed a preparation that gets {hold.credentials.join(', ')}. Start it to build; until
              then the last image is used.
            </p>
            <button class="btn p" type="button" onclick={() => void startHeld(form.path, hold.lending_hash)} disabled={busy !== '' || dirty}>
              {busy === `start:${form.path}` ? 'Starting…' : `Start the build, lending ${hold.credentials.join(', ')}`}
            </button>
          </div>
        {/if}
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
                  <button class="lnk" type="button" onclick={() => void build(item.repo_path)} disabled={busy !== '' || item.status === 'building'}>
                    {busy === `build:${item.repo_path}` ? 'Starting…' : 'Build again'}
                  </button>
                {/if}
              </li>
            {/each}
          </ul>
        {:else if form.source === 'dockerfile' && form.path}
          <p class="dim">
            Not built · builds on first use.
            {#if loaded.can_build}
              <button class="lnk" type="button" onclick={() => void build(form.path)} disabled={busy !== '' || dirty}>
                {busy === `build:${form.path}` ? 'Starting…' : 'Build now'}
              </button>{#if dirty}<small> Save first.</small>{/if}
            {:else}
              This runtime cannot build images.
            {/if}
          </p>
        {/if}

        {#if offering === form.path}
          <div class="offer">
            <span class="heading">Offer this recipe</span>
            <small>
              Sends its Dockerfile, preparation, checks and registries. Not its image, credentials or path.
            </small>
            <div class="grid">
              <label>
                <span>Channel</span>
                <select value={offerChannel} onchange={(event) => chooseChannel(event.currentTarget.value)} disabled={busy !== ''}>
                  {#each targets as target (target.channel)}
                    <option value={target.channel}>{target.channel}</option>
                  {/each}
                </select>
              </label>
              <label>
                <span>Repository</span>
                <input bind:value={offerRepo} spellcheck="false" placeholder="from the checkout's origin" disabled={busy !== ''} />
                <small>Its forge identity, like github.com/owner/name.</small>
              </label>
              <div class="wide sessions">
                <span class="heading">To</span>
                {#each offerPeers as peer (peer.id)}
                  <label class="toggle"><input type="checkbox" bind:group={offerNodes} value={peer.id} disabled={busy !== ''} /> {peer.name}</label>
                {/each}
              </div>
              <label class="wide">
                <span>Note</span>
                <input bind:value={offerNote} placeholder="optional, shown with the offer" disabled={busy !== ''} />
              </label>
            </div>
            <div class="acts">
              <button class="btn p" type="button" onclick={() => void sendOffer(form.path)} disabled={busy !== '' || !offerNodes.length}>
                {busy === `offer:${form.path}` ? 'Offering…' : 'Offer'}
              </button>
            </div>
            {#if offerResults.length}
              <ul class="builds">
                {#each offerResults as result (result.node)}
                  <li class:failed={result.state === 'refused'}><span>{offerResultLine(result)}</span></li>
                {/each}
              </ul>
            {/if}
          </div>
        {/if}

        <div class="acts">
          {#if targets.length && form.path && !dirty}
            <button class="lnk" type="button" onclick={() => startOffer(form.path)} disabled={busy !== ''}>
              {offering === form.path ? 'Do not offer' : 'Offer to…'}
            </button>
          {/if}
          {#if forms.length > 1}
            <button class="lnk" type="button" onclick={() => move(index, index - 1)} disabled={index === 0 || busy !== ''} aria-label="Move {form.path || 'this entry'} up">Move up</button>
            <button class="lnk" type="button" onclick={() => move(index, index + 1)} disabled={index === forms.length - 1 || busy !== ''} aria-label="Move {form.path || 'this entry'} down">Move down</button>
          {/if}
          <button class="lnk d" type="button" onclick={() => remove(index)} disabled={busy !== ''}>Remove this entry</button>
        </div>
      </details>
    {/each}

    <div class="acts">
      <button class="lnk" type="button" onclick={add} disabled={busy !== ''}>+ Add a repository</button>
      {#if dirty}
        <button class="btn p" type="button" onclick={() => void save()} disabled={busy !== '' || invalid.length > 0 || moved}>
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
  }
  input:not([type='checkbox']), select { min-height: 36px; }
  select { font-family: var(--sans); }
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
  .offer { display: grid; gap: 8px; margin-top: 12px; background: var(--s1); border-radius: 4px; padding: 10px 12px; }
  .offer input:not([type='checkbox']), .offer select { background: var(--s2); }
  .held { display: grid; gap: 8px; justify-items: start; margin-top: 12px; background: var(--s1); border-left: 3px solid var(--wait); padding: 8px 10px; }
  .held p { margin: 0; font-size: 12.5px; }
  .caution { margin: 0; color: var(--wait); font-size: 12.5px; }
  .bad, .error { color: var(--crit); }
  .dim, .note, .error { margin: 0; font-size: 12.5px; }
  .dim { color: var(--dim); }
  .note { color: var(--ok); }
</style>
