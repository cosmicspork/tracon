<script lang="ts">
  import { onMount } from 'svelte'
  import Card from './Card.svelte'
  import { api } from '../../lib/api'
  import { resetRefusal, serviceVolumeLine } from '../../lib/data'
  import { store } from '../../lib/store.svelte'
  import type { ServiceVolume } from '../../lib/types'

  let volumes = $state<ServiceVolume[] | null>(null)
  let error = $state('')
  let note = $state('')
  let confirming = $state('')
  let busy = $state('')

  const local = $derived(store.node?.loopback ?? false)

  async function load() {
    try {
      volumes = (await api.serviceVolumes()).volumes
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }

  async function reset(volume: ServiceVolume) {
    if (busy) return
    busy = volume.volume
    error = ''
    note = ''
    try {
      await api.resetServiceVolume(volume.volume)
      note = `Reset ${volume.repo}'s ${volume.service} data. Its next session starts it empty.`
      confirming = ''
      await load()
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }

  onMount(() => void load())
</script>

<Card
  title="Service data"
  note="Each repository's kept data for a catalogue service, such as a Postgres cluster. It survives sessions; only you reset it."
>
  {#if error}<p class="crit" role="alert">{error}</p>{/if}
  {#if note}<p class="ok">{note}</p>{/if}
  {#if !volumes}
    {#if !error}<p class="dim">Loading…</p>{/if}
  {:else if volumes.length === 0}
    <p class="dim">No repository keeps a service's data. Name the services under Repositories › Service data kept.</p>
  {:else}
    <ul>
      {#each volumes as v (v.volume)}
        {@const refusal = resetRefusal(v, local)}
        <li>
          <div class="line">
            <span class="repo">{v.repo}</span>
            {#if confirming === v.volume}
              <span class="acts">
                <button class="btn d" onclick={() => void reset(v)} disabled={busy !== ''}>{busy === v.volume ? 'Resetting…' : 'Reset, removing its data'}</button>
                <button class="lnk" onclick={() => (confirming = '')} disabled={busy !== ''}>Cancel</button>
              </span>
            {:else}
              <button class="btn" onclick={() => (confirming = v.volume)} disabled={refusal !== null || busy !== ''} title={refusal ?? undefined}>Reset</button>
            {/if}
          </div>
          <p class="how">{serviceVolumeLine(v, Date.now())}</p>
          {#if refusal && !v.live}<p class="how">{refusal}</p>{/if}
        </li>
      {/each}
    </ul>
  {/if}
</Card>

<style>
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 10px;
  }
  .line {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
  }
  .repo {
    font-weight: 600;
    overflow-wrap: anywhere;
    min-width: 0;
  }
  .acts {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .how {
    margin: 2px 0 0;
    color: var(--ink2);
    font-size: 0.9em;
  }
  .dim {
    color: var(--dim);
    margin: 0;
  }
  .crit {
    color: var(--crit);
    margin: 0;
  }
  .ok {
    margin: 0;
  }
</style>
