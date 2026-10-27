<script lang="ts">
  // The one non-subscription onboarding path: an OpenAI-compatible or
  // Anthropic-compatible upstream, keyed by an API key. Two calls in
  // sequence — seal the key first (`/api/credentials/import`), then register
  // the provider naming it (`POST /api/providers`) — so a failed second step
  // never leaves an orphaned, unfindable credential.
  import { api, ApiError } from '../../lib/api'
  import { keySegs } from '../../lib/config-form'
  import { providerChannelSeed } from '../../lib/channel'
  import { credentialImportToml, normalizeProviderName, PROVIDER_SHAPES } from '../../lib/providers'
  import { store } from '../../lib/store.svelte'

  let { onCancel, onCreated }: { onCancel: () => void; onCreated: (name: string) => void } = $props()

  let name = $state('')
  let shape = $state<string>(PROVIDER_SHAPES[0].value)
  let upstream = $state('')
  let apiKey = $state('')
  let busy = $state(false)
  let error = $state('')
  /** What the node refused, by the field it names (`name`, `upstream`,
      `providers.<name>.upstream`, a credential's name). */
  let fields = $state<Record<string, string[]>>({})
  function refusedAt(field: string): string[] {
    return fields[field] ?? []
  }

  const cleanName = $derived(normalizeProviderName(name))
  const valid = $derived(cleanName.length > 0 && upstream.trim().length > 0 && apiKey.trim().length > 0)

  async function submit() {
    if (!valid || busy) return
    busy = true
    error = ''
    fields = {}
    try {
      const channels = providerChannelSeed({
        nodeDefault: store.node?.default_channel,
        channels: store.channels,
        nodeId: store.node?.id ?? '',
      })
      const toml = credentialImportToml(cleanName, apiKey.trim(), channels)
      await api.importCredentials(toml)
      await api.createProvider({
        name: cleanName,
        upstream: upstream.trim(),
        shape,
        credential: cleanName,
      })
      onCreated(cleanName)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      if (e instanceof ApiError && e.fields?.length) {
        // The credential is named after the provider, so a refusal of
        // either name belongs under Name.
        const at: Record<string, string> = { name: 'name', credential: 'name', shape: 'shape', upstream: 'upstream' }
        const byField: Record<string, string[]> = {}
        let unplaced = false
        for (const p of e.fields) {
          const field = at[keySegs(p.field ?? p.path ?? '').at(-1) ?? '']
          const message = e.kind === 'hidden_character' ? `${p.message}: an invisible or control character. Retype it.` : p.message
          if (field) (byField[field] ??= []).push(message)
          else unplaced = true
        }
        fields = byField
        // Said once, next to its field, unless something has no field.
        if (!unplaced) error = ''
      }
    } finally {
      busy = false
    }
  }
</script>

<div class="form">
  <p class="note">Sealed on arrival; never shown again or passed to the harness.</p>
  <div class="form-grid">
    <label class="field">
      <span>Name</span>
      <input bind:value={name} placeholder="openrouter" spellcheck="false" disabled={busy} />
      <small>Lowercase · shown as {cleanName || '<name>'}/&lt;model&gt;</small>
      {#each refusedAt('name') as message (message)}<small class="bad" role="alert">{message}</small>{/each}
    </label>
    <label class="field">
      <span>Shape</span>
      <select bind:value={shape} disabled={busy}>
        {#each PROVIDER_SHAPES as s (s.value)}
          <option value={s.value}>{s.label}</option>
        {/each}
      </select>
      <small>Sets the key's headers and paths.</small>
      {#each refusedAt('shape') as message (message)}<small class="bad" role="alert">{message}</small>{/each}
    </label>
  </div>
  <label class="field">
    <span>Upstream</span>
    <input bind:value={upstream} placeholder="https://openrouter.ai/api/v1" spellcheck="false" disabled={busy} />
    <small>Added to the egress allowlist.</small>
    {#each refusedAt('upstream') as message (message)}<small class="bad" role="alert">{message}</small>{/each}
  </label>
  <label class="field">
    <span>API key</span>
    <input type="password" bind:value={apiKey} disabled={busy} autocomplete="off" />
  </label>
  {#if error}<small class="bad">{error}</small>{/if}
  <div class="form-acts">
    <button class="lnk" type="button" onclick={onCancel} disabled={busy}>Cancel</button>
    <button class="btn p" type="button" onclick={submit} disabled={!valid || busy}>
      {busy ? 'Saving…' : 'Save provider'}
    </button>
  </div>
</div>

<style>
  .form {
    display: grid;
    gap: 12px;
    background: var(--wash-dim);
    border-radius: 4px;
    padding: 16px;
  }
  .note {
    margin: 0;
    max-width: 62ch;
    color: var(--ink2);
    font-size: 13px;
  }
  .form-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
  }
  .field {
    display: grid;
    gap: 5px;
    min-width: 0;
  }
  .field > span {
    font: 500 12px var(--sans);
    color: var(--ink2);
  }
  .field input,
  .field select {
    background: var(--s3);
    border: 1px solid var(--rule);
    border-radius: 4px;
    color: var(--ink);
    padding: 0 10px;
    font: 400 13px var(--sans);
    height: 34px;
    box-sizing: border-box;
  }
  .field small {
    color: var(--dim);
    font-size: 11.5px;
  }
  .bad {
    color: var(--crit);
  }
  .form-acts {
    display: flex;
    align-items: center;
    gap: 14px;
    justify-content: flex-end;
  }
  @media (max-width: 700px) {
    .form-grid {
      grid-template-columns: 1fr;
    }
  }
</style>
