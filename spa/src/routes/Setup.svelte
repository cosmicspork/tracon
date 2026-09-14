<script lang="ts">
  import { onMount } from 'svelte'
  import {
    blocked,
    canRestartNode,
    installCli,
    installService,
    openNode,
    restartNode,
    setupStatus,
    setupSteps,
    type SetupStatus,
  } from '../lib/desktop-setup'

  let status = $state<SetupStatus | null>(null)
  let busy = $state<'service' | 'cli' | 'restart' | null>(null)
  let error = $state<string | null>(null)

  const steps = $derived(status ? setupSteps(status) : [])
  const stuck = $derived(blocked(steps))

  const message = (e: unknown) => (e instanceof Error ? e.message : String(e))

  async function refresh() {
    try {
      status = await setupStatus()
    } catch (e) {
      error = message(e)
    }
  }

  async function run(what: 'service' | 'cli' | 'restart', f: () => Promise<SetupStatus>) {
    busy = what
    error = null
    try {
      status = await f()
    } catch (e) {
      error = message(e)
    } finally {
      busy = null
    }
  }

  onMount(() => {
    refresh()
    const timer = setInterval(() => {
      if (busy === null) refresh()
    }, 3000)
    return () => clearInterval(timer)
  })
</script>

<main>
  <h1>tracon</h1>
  <p class="lede">
    The node runs in the background under your user's service manager. This app installs it, keeps it
    up to date, and opens its interface.
  </p>

  {#if status === null}
    <p class="dim">Looking at this machine…</p>
  {:else}
    <ol>
      {#each steps as s (s.id)}
        <li class:done={s.done}>
          <span class="mark">{s.done ? '✓' : '·'}</span>
          <div>
            <strong>{s.title}</strong>
            <small>{s.detail}</small>
            {#if s.reason}
              <code class="reason">{s.reason}</code>
            {/if}
            {#if s.command}
              <code>{s.command}</code>
            {/if}
            {#if s.id === 'service' && (canRestartNode(status) || (!s.done && status.owner !== 'foreign'))}
              <div class="row">
                {#if canRestartNode(status)}
                  <button class="btn" class:p={!s.done} disabled={busy !== null} onclick={() => run('restart', restartNode)}>
                    {busy === 'restart' ? 'Restarting…' : 'Restart node'}
                  </button>
                {/if}
                {#if !s.done && status.owner !== 'foreign'}
                  <button class="btn" class:p={!status.service_installed} disabled={busy !== null || stuck} onclick={() => run('service', installService)}>
                    {busy === 'service' ? 'Installing…' : status.service_installed ? 'Reinstall and start' : 'Install and start'}
                  </button>
                {/if}
              </div>
            {:else if s.id === 'cli' && status.cli_version !== status.sidecar_version}
              <button class="btn" disabled={busy !== null} onclick={() => run('cli', installCli)}>
                {busy === 'cli' ? 'Installing…' : 'Install'}
              </button>
            {/if}
          </div>
        </li>
      {/each}
    </ol>
  {/if}

  {#if error}
    <p class="err">{error}</p>
  {/if}

  {#if status && status.owner !== 'none'}
    <div class="acts">
      <button class="btn p" onclick={() => openNode()}>Open tracon</button>
    </div>
  {/if}
</main>

<style>
  main {
    max-width: 560px;
    margin: 56px auto;
    padding: 0 20px;
    display: grid;
    gap: 16px;
  }
  h1 {
    margin: 0;
    font-size: 20px;
  }
  .lede,
  .dim {
    margin: 0;
    color: var(--dim);
  }
  ol {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 10px;
  }
  li {
    display: grid;
    grid-template-columns: 20px minmax(0, 1fr);
    gap: 10px;
    background: var(--s1);
    border-radius: 4px;
    padding: 12px 14px;
  }
  li > div {
    display: grid;
    gap: 6px;
    justify-items: start;
  }
  .mark {
    color: var(--dim);
  }
  li.done .mark {
    color: var(--ink);
  }
  small {
    color: var(--dim);
  }
  code {
    font: 12px var(--mono);
    background: var(--s0);
    padding: 4px 6px;
    border-radius: 3px;
    user-select: all;
    overflow-wrap: anywhere;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  code.reason {
    color: var(--crit);
  }
  .err {
    margin: 0;
    color: var(--crit);
  }
</style>
