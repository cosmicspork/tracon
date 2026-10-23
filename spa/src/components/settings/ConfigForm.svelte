<script lang="ts">
  // Every node.toml table the bespoke screens do not edit, as a form built
  // from the node's description of each key. A table saves on its own: its
  // changed keys go as one request of per-key operations, decided against
  // the version this form read. Off the node's machine the node confirms the
  // save with a passkey (the prompt names the keys); a version that moved
  // underneath is read again and the operator decides where both changed.
  import ConfigField from './ConfigField.svelte'
  import { api } from '../../lib/api'
  import {
    BESPOKE,
    blankEntry,
    contextOf,
    entriesOps,
    entryStates,
    keySegs,
    mapOps,
    mapRows,
    rebase,
    rebaseEntries,
    rebaseMap,
    saveOps,
    sectionOps,
    sections,
    shown,
    entryValue,
    type ArrayField,
    type Clash,
    type Draft,
    type EntryState,
    type Field,
    type MapField,
    type MapRow,
    type Section,
  } from '../../lib/config-form'
  import { store } from '../../lib/store.svelte'
  import type { ConfigOp, NodeConfig } from '../../lib/types'

  // `nodeId` names a peer to edit instead of the serving node: read and
  // saved over the mesh, confirmed here, and only the keys that peer lets an
  // admin peer change are offered (`editable_by_peer`).
  let { onsaved, nodeId = '' }: { onsaved?: () => void; nodeId?: string } = $props()

  const local = $derived(nodeId ? false : (store.node?.loopback ?? false))
  const channels = $derived(store.channels.filter((c) => !c.archived).map((c) => c.name))

  let cfg = $state<NodeConfig | null>(null)
  let loadError = $state('')
  let drafts = $state<Record<string, Draft>>({})
  let mapDrafts = $state<Record<string, MapRow[]>>({})
  let entryDrafts = $state<Record<string, EntryState[]>>({})
  let fieldErrors = $state<Record<string, string[]>>({})
  let notes = $state<Record<string, { tone: 'ok' | 'crit' | 'dim'; text: string }>>({})
  let clashes = $state<Record<string, Clash[]>>({})
  let busy = $state('')
  let filter = $state('')
  let opened = $state<Record<string, boolean>>({})

  // Values the schema cannot list because the node holds them.
  function choices(key: string): string[] | undefined {
    if (key === 'session.default_channel') return ['', ...channels]
    if (key === 'harness.id') return ['claude', 'native']
    return undefined
  }

  const ctx = $derived(cfg ? contextOf(cfg, local, choices) : null)
  const all = $derived(ctx ? sections(ctx) : [])
  const owed = $derived(new Set((store.restart?.config?.keys ?? []).map((k) => keySegs(k).join('.'))))
  const needle = $derived(filter.trim().toLowerCase())

  function hit(text: string): boolean {
    return !needle || text.toLowerCase().includes(needle)
  }
  function visibleFields(sec: Section): Field[] {
    return sec.fields.filter((f) => hit(f.key) || hit(f.label) || hit(sec.id))
  }
  function sectionShown(sec: Section): boolean {
    return (
      visibleFields(sec).length > 0 ||
      sec.maps.some((m) => hit(m.key)) ||
      sec.arrays.some((a) => hit(a.key) || a.fields.some((f) => hit(`${a.key}.${f}`)))
    )
  }

  // A peer's keys as this form decides them: editable only where the peer
  // lets an admin peer change them, with the peer's reason where not.
  function asPeer(c: NodeConfig): NodeConfig {
    return {
      ...c,
      keys: c.keys.map((k) => ({
        ...k,
        editable: k.editable_by_peer === true,
        why_not: k.why_not_by_peer ?? k.why_not,
      })),
    }
  }

  async function load(): Promise<NodeConfig | null> {
    try {
      cfg = nodeId ? asPeer(await api.nodeConfig(nodeId)) : await api.config()
      loadError = ''
      return cfg
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e)
      return null
    }
  }
  void load()

  function entries(array: ArrayField): EntryState[] {
    return entryDrafts[array.key] ?? (ctx ? entryStates(ctx, array) : [])
  }
  function rows(map: MapField): MapRow[] {
    return mapDrafts[map.key] ?? mapRows(map)
  }

  /** What saving a section would send, where each operation's subject is
      on screen, and what is not fit to send yet. */
  function pending(sec: Section): { ops: ConfigOp[]; screen: (string | undefined)[]; errors: Record<string, string> } {
    const out = sectionOps(sec.fields, drafts)
    const ops = [...out.ops]
    const screen: (string | undefined)[] = ops.map(() => undefined)
    const errors = { ...out.errors }
    for (const map of sec.maps) {
      if (!mapDrafts[map.key]) continue
      const made = mapOps(map, mapDrafts[map.key])
      ops.push(...made)
      screen.push(...made.map(() => map.key))
    }
    for (const array of sec.arrays) {
      if (!entryDrafts[array.key] || !ctx) continue
      const made = entriesOps(array, entryStates(ctx, array), entryDrafts[array.key])
      ops.push(...made.ops)
      screen.push(...made.screen)
      Object.assign(errors, made.errors)
    }
    return { ops, screen, errors }
  }

  function setDraft(key: string, draft: Draft | undefined) {
    if (draft) drafts[key] = draft
    else delete drafts[key]
    delete fieldErrors[key]
  }

  function editRows(map: MapField, edit: (rows: MapRow[]) => MapRow[]) {
    mapDrafts[map.key] = edit(rows(map).map((r) => ({ ...r })))
  }

  function editEntries(array: ArrayField, edit: (states: EntryState[]) => EntryState[]) {
    entryDrafts[array.key] = edit([...entries(array)])
  }

  function setEntryDraft(array: ArrayField, at: number, name: string, draft: Draft | undefined) {
    editEntries(array, (states) => {
      const next = { ...states[at], drafts: { ...states[at].drafts } }
      if (draft) next.drafts[name] = draft
      else delete next.drafts[name]
      states[at] = next
      return states
    })
    delete fieldErrors[`${array.key}[${at}].${name}`]
  }

  function move(array: ArrayField, from: number, to: number) {
    editEntries(array, (states) => {
      const [moved] = states.splice(from, 1)
      states.splice(to, 0, moved)
      return states
    })
  }

  function discard(sec: Section) {
    for (const f of sec.fields) delete drafts[f.key]
    for (const m of sec.maps) delete mapDrafts[m.key]
    for (const a of sec.arrays) delete entryDrafts[a.key]
    for (const key of Object.keys(fieldErrors)) if (inSection(sec, key)) delete fieldErrors[key]
    delete clashes[sec.id]
    delete notes[sec.id]
  }

  function inSection(sec: Section, key: string): boolean {
    return sec.fields.some((f) => f.key === key) || sec.arrays.some((a) => key.startsWith(`${a.key}[`)) || sec.maps.some((m) => key.startsWith(`${m.key}.`))
  }

  function fieldKeys(sec: Section): string[] {
    const keys = sec.fields.map((f) => f.key)
    for (const a of sec.arrays) {
      keys.push(a.key)
      entries(a).forEach((_, i) => keys.push(...a.fields.map((name) => `${a.key}[${i}].${name}`)))
    }
    for (const m of sec.maps) keys.push(m.key)
    return keys
  }

  async function save(sec: Section) {
    if (!cfg || !ctx || busy) return
    const { ops, screen, errors } = pending(sec)
    if (Object.keys(errors).length) {
      for (const [key, message] of Object.entries(errors)) fieldErrors[key] = [message]
      notes[sec.id] = { tone: 'crit', text: 'Some values do not read yet; they are marked below.' }
      return
    }
    if (!ops.length) return
    busy = sec.id
    delete notes[sec.id]
    const outcome = await saveOps((hash, o) => (nodeId ? api.patchNodeConfig(nodeId, hash, o) : api.patchConfig(hash, o)), cfg.hash, ops, fieldKeys(sec), screen)
    try {
      if (outcome.kind === 'saved') {
        const { result } = outcome
        discard(sec)
        // Every other table's edits are carried onto the file as it is now,
        // so the next save, decided against the new version, writes only
        // what was typed there.
        await reload(sec.id)
        if (!nodeId) void store.refreshRestart()
        onsaved?.()
        notes[sec.id] = {
          tone: 'ok',
          text: !result.changed.length
            ? 'Nothing changed: node.toml already says this.'
            : result.restart_required
              ? `Saved ${result.changed.join(', ')}. The node applies it when it restarts; the banner above offers that.`
              : `Saved ${result.changed.join(', ')}. The running node uses it from now on.`,
        }
      } else if (outcome.kind === 'conflict') {
        await reload()
        notes[sec.id] = {
          tone: 'dim',
          text: 'node.toml changed on the node while you were editing. It has been read again; your edits are kept. Check them and save again.',
        }
      } else if (outcome.kind === 'refused') {
        fieldErrors = { ...fieldErrors, ...outcome.fields }
        notes[sec.id] = { tone: 'crit', text: outcome.general.join(' ') || 'Refused: the keys it is about are marked below.' }
      } else {
        notes[sec.id] = { tone: 'crit', text: outcome.message }
      }
    } finally {
      busy = ''
    }
  }

  /**
   * Read node.toml again and carry every edit onto it: after a conflict,
   * and after any save, so a draft is always decided against the version
   * the next save names. A key's draft stays as typed; an array's entries
   * and a map's names keep only what the operator changed, over what the
   * node holds now (`rebaseEntries`, `rebaseMap`). Where the node changed
   * what was edited here too, the clash is named and Save waits for a
   * choice. `saved` is the table just saved, whose own drafts are gone.
   */
  async function reload(saved?: string) {
    if (!ctx) {
      await load()
      return
    }
    const before = all.flatMap((s) => s.fields)
    const wasArrays = new Map(all.flatMap((s) => s.arrays).map((a) => [a.key, a.entries]))
    const wasMaps = new Map(all.flatMap((s) => s.maps).map((m) => [m.key, m.entries]))
    const next = await load()
    if (!next || !ctx) return
    const now = sections(ctx)
    const carried = rebase(before, now.flatMap((s) => s.fields), drafts)
    drafts = carried.drafts
    const grouped: Record<string, Clash[]> = {}
    for (const clash of carried.clashes) {
      const sec = now.find((s) => s.fields.some((f) => f.key === clash.key))
      if (sec) (grouped[sec.id] ??= []).push(clash)
    }
    for (const sec of now) {
      for (const array of sec.arrays) {
        if (!entryDrafts[array.key]) continue
        const out = rebaseEntries(ctx, array, wasArrays.get(array.key) ?? [], entryDrafts[array.key])
        entryDrafts[array.key] = out.states
        if (out.clashes.length) (grouped[sec.id] ??= []).push(...out.clashes)
      }
      for (const map of sec.maps) {
        if (!mapDrafts[map.key]) continue
        const out = rebaseMap(map, wasMaps.get(map.key) ?? [], mapDrafts[map.key])
        mapDrafts[map.key] = out.rows
        if (out.clashes.length) (grouped[sec.id] ??= []).push(...out.clashes)
      }
    }
    // A clash not yet decided stays, unless its table was just saved.
    for (const [id, list] of Object.entries(clashes)) {
      if (id === saved) continue
      for (const clash of list) if (!grouped[id]?.some((c) => c.key === clash.key)) (grouped[id] ??= []).push(clash)
    }
    clashes = grouped
  }

  /** Take the node's side of every clash in a table; the other edits stay. */
  function takeTheirs(sec: Section) {
    const gone: Record<string, number[]> = {}
    for (const clash of clashes[sec.id] ?? []) {
      const array = sec.arrays.find((a) => clash.key.startsWith(`${a.key}[`))
      const map = sec.maps.find((m) => clash.key.startsWith(`${m.key}.`))
      if (array) {
        const at = clash.key.slice(array.key.length).match(/^\[(\d+)\](?:\.(.+))?$/)
        if (!at) continue
        if (at[2]) {
          const states = entries(array)
          const i = Number(at[1])
          if (states[i]) setEntryDraft(array, i, at[2], undefined)
        } else {
          ;(gone[array.key] ??= []).push(Number(at[1]))
        }
      } else if (map) {
        const name = clash.key.slice(map.key.length + 1)
        const theirs = mapRows(map).find((r) => r.name === name)
        editRows(map, (r) => {
          const rest = r.filter((row) => row.name.trim() !== name)
          return theirs ? [...rest, theirs] : rest
        })
      } else {
        delete drafts[clash.key]
      }
    }
    for (const array of sec.arrays) {
      const drop = new Set(gone[array.key] ?? [])
      if (drop.size) editEntries(array, (states) => states.filter((_, i) => !drop.has(i)))
    }
    delete clashes[sec.id]
    notes[sec.id] = { tone: 'dim', text: 'The node’s values are kept; your other edits are still here.' }
  }

  function title(sec: Section): string {
    if (sec.arrays.length && !sec.fields.length && !sec.maps.length) return `[[${sec.id}]]`
    return sec.id ? `[${sec.id}]` : 'Top of the file'
  }

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text)
    } catch {
      // The text is on screen to select by hand.
    }
  }
</script>

<div class="config" data-testid="config-form">
  <p class="lede">
    Every key of node.toml the node reads, as {nodeId ? 'that peer' : (store.node?.name ?? 'the node')} describes it. A table saves on its
    own; <b class="live">live</b> keys apply to the running node at once, the rest when it restarts.
    {#if nodeId}Each save is confirmed here, with a prompt naming the peer and the keys, and checked again there.
    {:else if !local}Off this machine each save is confirmed with your passkey, and the prompt names the keys it changes.{/if}
    Repositories are under <a href="#channels">{BESPOKE.repo.split(' › ')[0]}</a> and providers under
    <a href="#connections">{BESPOKE.providers.split(' › ')[0]}</a>.
  </p>

  {#if !cfg && loadError}
    <p class="why"><b>node.toml could not be read</b><i>{loadError}</i></p>
  {:else if !cfg}
    <div class="empty">Reading node.toml…</div>
  {:else}
    <label class="find">
      <span>Find a key</span>
      <input type="search" bind:value={filter} placeholder="budget, gh, kubernetes…" spellcheck="false" autocomplete="off" />
    </label>

    {#each all as sec (sec.id)}
      {#if sectionShown(sec)}
        {@const plan = pending(sec)}
        {@const count = plan.ops.length + Object.keys(plan.errors).length}
        {@const fields = visibleFields(sec)}
        <details
          class="section"
          open={!!needle || opened[sec.id] || count > 0 || !!notes[sec.id]}
          ontoggle={(e) => (opened[sec.id] = e.currentTarget.open)}
          data-section={sec.id}
        >
          <summary>
            <code>{title(sec)}</code>
            <small>{sec.fields.length + sec.maps.length + sec.arrays.length} key{sec.fields.length + sec.maps.length + sec.arrays.length === 1 ? '' : 's'}</small>
            {#if count}<span class="unsaved">{count} unsaved</span>{/if}
          </summary>

          {#if clashes[sec.id]?.length}
            <div class="clash" role="alert">
              <b>Changed on the node too</b>
              <ul>
                {#each clashes[sec.id] as clash (clash.key)}
                  <li>
                    <code>{clash.key}</code>:
                    {clash.says ?? `now ${shown(clash.theirs)} on the node; saving writes ${shown(clash.yours.reset ? undefined : clash.yours.v)}.`}
                  </li>
                {/each}
              </ul>
              <div class="acts">
                <button class="btn" type="button" onclick={() => delete clashes[sec.id]}>Keep mine</button>
                <button class="lnk" type="button" onclick={() => takeTheirs(sec)}>Use theirs</button>
              </div>
            </div>
          {/if}

          {#if fields.length}
            <div class="grid">
              {#each fields as field (field.key)}
                <ConfigField
                  {field}
                  draft={drafts[field.key]}
                  errors={fieldErrors[field.key] ?? []}
                  disabled={busy !== ''}
                  owed={owed.has(field.segs.join('.'))}
                  onchange={(d) => setDraft(field.key, d)}
                />
              {/each}
            </div>
          {/if}

          {#each sec.maps as map (map.key)}
            {@const list = rows(map)}
            <div class="map" data-key={map.key}>
              <div class="head">
                <span class="name">{map.label}</span>
                <span class="badge" class:live={map.spec.apply === 'live'}>{map.spec.apply === 'live' ? 'live' : 'needs restart'}</span>
              </div>
              <code class="key">{map.key}.&lt;name&gt;</code>
              {#if !map.editable && map.whyNot}<small class="why">{map.whyNot}</small>{/if}
              {#each list as row, i (i)}
                <div class="map-row">
                  <input
                    aria-label="{map.label}: name"
                    value={row.name}
                    disabled={!map.editable || busy !== ''}
                    spellcheck="false"
                    placeholder="name"
                    oninput={(e) => editRows(map, (r) => ((r[i].name = e.currentTarget.value), r))}
                  />
                  {#if map.control === 'lines'}
                    <textarea
                      aria-label="{map.label}: {row.name || 'value'}"
                      rows="2"
                      value={row.v}
                      disabled={!map.editable || busy !== ''}
                      spellcheck="false"
                      placeholder="one per line"
                      oninput={(e) => editRows(map, (r) => ((r[i].v = e.currentTarget.value), r))}
                    ></textarea>
                  {:else}
                    <input
                      aria-label="{map.label}: {row.name || 'value'}"
                      value={row.v}
                      disabled={!map.editable || busy !== ''}
                      spellcheck="false"
                      oninput={(e) => editRows(map, (r) => ((r[i].v = e.currentTarget.value), r))}
                    />
                  {/if}
                  <button class="lnk d" type="button" disabled={!map.editable || busy !== ''} onclick={() => editRows(map, (r) => r.filter((_, at) => at !== i))}>Remove</button>
                </div>
              {:else}
                <small>None.</small>
              {/each}
              {#each fieldErrors[map.key] ?? [] as message (message)}<small class="bad" role="alert">{message}</small>{/each}
              {#if map.editable}
                <button class="lnk" type="button" disabled={busy !== ''} onclick={() => editRows(map, (r) => [...r, { name: '', v: '' }])}>+ Add</button>
              {/if}
            </div>
          {/each}

          {#each sec.arrays as array (array.key)}
            {@const states = entries(array)}
            <div class="entries" data-key={array.key}>
              {#each states as state, i (i)}
                {@const name = entryValue(state).entry[array.fields[0]]}
                <fieldset class="entry">
                  <legend><code>[[{array.key}]]</code> {name ? String(name) : state.origin === null ? 'new entry' : `entry ${i + 1}`}</legend>
                  <div class="grid">
                    {#each state.fields as field, j (field.key)}
                      <ConfigField
                        {field}
                        draft={state.drafts[array.fields[j]]}
                        errors={fieldErrors[`${array.key}[${i}].${array.fields[j]}`] ?? []}
                        disabled={busy !== ''}
                        onchange={(d) => setEntryDraft(array, i, array.fields[j], d)}
                      />
                    {/each}
                  </div>
                  <div class="acts">
                    <button class="lnk" type="button" disabled={i === 0 || busy !== ''} onclick={() => move(array, i, i - 1)} aria-label="Move entry {i + 1} up">Move up</button>
                    <button class="lnk" type="button" disabled={i === states.length - 1 || busy !== ''} onclick={() => move(array, i, i + 1)} aria-label="Move entry {i + 1} down">Move down</button>
                    <button class="lnk d" type="button" disabled={busy !== ''} onclick={() => editEntries(array, (s) => s.filter((_, at) => at !== i))}>Remove this entry</button>
                  </div>
                </fieldset>
              {:else}
                <small>No entries.</small>
              {/each}
              {#each fieldErrors[array.key] ?? [] as message (message)}<small class="bad" role="alert">{message}</small>{/each}
              <button class="lnk" type="button" disabled={busy !== '' || !ctx} onclick={() => ctx && editEntries(array, (s) => [...s, blankEntry(ctx, array, 1000 + s.length)])}>+ Add an entry</button>
            </div>
          {/each}

          <div class="acts save">
            <button class="btn p" type="button" disabled={!count || busy !== '' || !!clashes[sec.id]?.length} onclick={() => void save(sec)}>
              {busy === sec.id ? 'Saving…' : `Save ${title(sec)}`}
            </button>
            {#if count}<button class="lnk" type="button" disabled={busy !== ''} onclick={() => discard(sec)}>Discard changes</button>{/if}
            {#if notes[sec.id]}
              <small class="note {notes[sec.id].tone}" role={notes[sec.id].tone === 'crit' ? 'alert' : 'status'}>{notes[sec.id].text}</small>
            {/if}
          </div>
        </details>
      {/if}
    {/each}

    <details class="section advanced" data-testid="config-advanced">
      <summary><code>Advanced</code><small>node.toml as the node reads it</small></summary>
      <p class="lede">
        Read-only. Secrets, and keys this node does not read, are shown as <code>{cfg.redacted}</code>; the file's
        comments are left out. A save is decided against this version.
      </p>
      <dl class="facts">
        <div><dt>Version</dt><dd><code data-testid="config-hash">{cfg.hash}</code></dd></div>
        {#if cfg.running_hash}<div><dt>Running</dt><dd><code>{cfg.running_hash}</code>{cfg.running_hash === cfg.hash ? ' · the same' : ''}</dd></div>{/if}
        <div><dt>File</dt><dd><code>{cfg.readonly.config_path}</code></dd></div>
      </dl>
      <pre class="toml" aria-label="node.toml, redacted">{cfg.toml}</pre>
      <div class="acts"><button class="lnk" type="button" onclick={() => void copy(cfg?.toml ?? '')}>Copy</button></div>
    </details>
  {/if}
</div>

<style>
  .config {
    display: grid;
    gap: 10px;
    min-width: 0;
  }
  .lede {
    margin: 0;
    font-size: 13px;
    color: var(--ink2);
    max-width: 72ch;
  }
  .lede .live {
    color: var(--ok);
    font-weight: 500;
  }
  .find {
    display: grid;
    gap: 5px;
    max-width: 420px;
  }
  .find > span,
  .facts dt {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  input,
  textarea {
    background: var(--s2);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--mono);
    min-width: 0;
    min-height: 36px;
    box-sizing: border-box;
  }
  input:disabled,
  textarea:disabled {
    opacity: 0.55;
  }
  .section {
    background: var(--s1);
    border-radius: 6px;
    padding: 10px 14px;
    min-width: 0;
  }
  .section > summary {
    cursor: pointer;
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 12px;
    min-height: 32px;
    align-content: center;
  }
  .section > summary code {
    font: 500 13.5px var(--mono);
    color: var(--ink);
    overflow-wrap: anywhere;
  }
  .section > summary::-webkit-details-marker {
    display: none;
  }
  .section > summary {
    list-style: none;
  }
  .section > summary::before {
    content: '▸';
    color: var(--dim);
    font-size: 11px;
    width: 10px;
  }
  .section[open] > summary::before {
    content: '▾';
  }
  .section > summary:focus-visible {
    outline: 2px solid var(--acc);
    outline-offset: 2px;
    border-radius: 3px;
  }
  .section[open] > summary {
    margin-bottom: 10px;
  }
  .unsaved {
    font: 500 11px var(--mono);
    color: var(--acc);
  }
  .grid {
    display: grid;
    gap: 10px;
    grid-template-columns: repeat(auto-fill, minmax(min(100%, 260px), 1fr));
    align-items: start;
  }
  small {
    font-size: 12.5px;
    color: var(--dim);
  }
  .bad {
    color: var(--crit);
  }
  .why {
    color: var(--ink2);
  }
  .acts {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 14px;
  }
  .acts.save {
    margin-top: 12px;
  }
  .note.ok {
    color: var(--ok);
  }
  .note.crit {
    color: var(--crit);
  }
  .note.dim {
    color: var(--ink2);
  }
  .clash {
    display: grid;
    gap: 6px;
    margin-bottom: 10px;
    padding: 8px 10px;
    background: var(--wash-wait);
    border-left: 3px solid var(--wait);
    font-size: 13px;
  }
  .clash ul {
    margin: 0;
    padding-left: 18px;
    overflow-wrap: anywhere;
  }
  .map,
  .entries {
    display: grid;
    gap: 6px;
    margin-top: 10px;
    padding: 8px 10px;
    border-radius: 4px;
    background: var(--s2);
    min-width: 0;
  }
  .map .head {
    display: flex;
    flex-wrap: wrap;
    justify-content: space-between;
    gap: 2px 8px;
  }
  .map .name {
    font: 500 13.5px var(--sans);
  }
  .key {
    font: 11.5px var(--mono);
    color: var(--dim);
    overflow-wrap: anywhere;
  }
  .badge {
    font: 500 10.5px var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--wait);
  }
  .badge.live {
    color: var(--ok);
  }
  .map-row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 2fr) auto;
    gap: 6px;
    align-items: start;
  }
  .map-row input,
  .map-row textarea {
    background: var(--s1);
  }
  .entries {
    background: transparent;
    padding: 0;
  }
  .entry {
    margin: 0;
    border: 1px solid var(--rule);
    border-radius: 4px;
    padding: 8px 10px 10px;
    display: grid;
    gap: 8px;
    min-width: 0;
  }
  .entry legend {
    padding: 0 4px;
    font-size: 13px;
    overflow-wrap: anywhere;
  }
  .facts {
    margin: 8px 0;
    display: grid;
    gap: 4px;
  }
  .facts div {
    display: grid;
    grid-template-columns: 80px minmax(0, 1fr);
    gap: 10px;
  }
  .facts dd {
    margin: 0;
    overflow-wrap: anywhere;
    font-size: 12.5px;
  }
  .facts code {
    font: 12px var(--mono);
  }
  .toml {
    margin: 0;
    padding: 10px 12px;
    background: var(--s2);
    border-radius: 4px;
    font: 12px/1.5 var(--mono);
    color: var(--ink);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  @media (max-width: 520px) {
    .map-row {
      grid-template-columns: minmax(0, 1fr) auto;
    }
    .map-row textarea,
    .map-row input:nth-child(2) {
      grid-column: 1 / -1;
      grid-row: 2;
    }
  }
</style>
