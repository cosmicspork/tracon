<script lang="ts">
  // The one non-subscription onboarding path: an OpenAI-compatible or
  // Anthropic-compatible upstream, keyed by an API key. Two calls in
  // sequence — seal the key first (`/api/credentials/import`), then register
  // the provider naming it (`POST /api/providers`) — so a failed second step
  // never leaves an orphaned, unfindable credential.
  import { api } from '../../lib/api'
  import { credentialImportToml, normalizeProviderName, PROVIDER_SHAPES } from '../../lib/providers'
  import { store } from '../../lib/store.svelte'

  let { onCancel, onCreated }: { onCancel: () => void; onCreated: (name: string) => void } = $props()

  let name = $state('')
  let shape = $state<string>(PROVIDER_SHAPES[0].value)
  let upstream = $state('')
  let apiKey = $state('')
  let busy = $state(false)
  let error = $state('')

  const cleanName = $derived(normalizeProviderName(name))
  const valid = $derived(cleanName.length > 0 && upstream.trim().length > 0 && apiKey.trim().length > 0)

  async function submit() {
    if (!valid || busy) return
    busy = true
    error = ''
    try {
      const channels = store.channels.filter((c) => !c.archived).map((c) => c.name)
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
    } finally {
      busy = false
    }
  }
</script>

<div class="form">
  <p class="note">
    The key is sealed on arrival and never shown again. tracon injects it; the harness never sees it directly.
  </p>
  <div class="form-grid">
    <label class="field">
      <span>Name</span>
      <input bind:value={name} placeholder="openrouter" spellcheck="false" disabled={busy} />
      <small>Lowercase, how it appears in the model picker: {cleanName || '<name>'}/&lt;model&gt;.</small>
    </label>
    <label class="field">
      <span>Shape</span>
      <select bind:value={shape} disabled={busy}>
        {#each PROVIDER_SHAPES as s (s.value)}
          <option value={s.value}>{s.label}</option>
        {/each}
      </select>
      <small>Which headers and paths the key becomes.</small>
    </label>
  </div>
  <label class="field">
    <span>Upstream</span>
    <input bind:value={upstream} placeholder="https://openrouter.ai/api/v1" spellcheck="false" disabled={busy} />
    <small>Added to the egress allowlist automatically if it isn't already there.</small>
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
