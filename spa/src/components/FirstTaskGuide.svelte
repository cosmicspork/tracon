<script lang="ts">
  import { formatTokens } from '../lib/format'
  import { nodeReadiness } from '../lib/nodes'
  import { store } from '../lib/store.svelte'
  import { isTerminal, type ChannelInfo, type NodeInfo } from '../lib/types'

  let {
    local,
    localChannels = [],
    peers,
    selectedNodeId = null,
    selectedChannel = null,
    channel = null,
    onchoose,
  }: {
    local: NodeInfo | null
    localChannels?: string[]
    peers: { node: NodeInfo; channel: string }[]
    selectedNodeId?: string | null
    selectedChannel?: string | null
    channel?: ChannelInfo | null
    onchoose: (nodeId: string, channel: string) => void
  } = $props()

  const localReadiness = $derived(local ? nodeReadiness(local) : null)
  const selected = $derived(
    [
      ...(local ? localChannels.map((channel) => ({ node: local, channel })) : []),
      ...peers,
    ].find((target) => target.node.id === selectedNodeId && target.channel === selectedChannel) ?? null,
  )
  const latest = $derived(selected
    ? [...store.sessions.values()]
      .filter((session) => session.node_id === selected.node.id && session.channel === selected.channel)
      .sort((a, b) => b.created_ms - a.created_ms)[0] ?? null
    : null)
  const dailyLimit = $derived.by(() => {
    const activeChannel = channel
    const ceiling = activeChannel?.ceiling.ceiling
    if (!activeChannel || ceiling === null || ceiling === undefined) return 'No daily channel ceiling is configured.'
    const { usage_today: usage, state } = activeChannel.ceiling
    return `${formatTokens(usage)} of ${formatTokens(ceiling)} tokens today${state === 'at' ? ' — new sessions are refused' : ''}.`
  })
</script>

<section class="guide" aria-labelledby="first-task-title">
  <div class="heading">
    <div>
      <span class="eyebrow">First task</span>
      <h2 id="first-task-title">Choose where it runs</h2>
    </div>
    {#if latest}
      <a class="session" href="/sessions/{latest.id}">
        {isTerminal(latest.state) ? 'Latest session ended' : 'Latest session is active'} · open session
      </a>
    {/if}
  </div>

  <div class="targets">
    <div class="target-group">
      <span class="label">Run here</span>
      {#if local && localReadiness?.canRun && localChannels.length}
        <div class="peer-list">
          {#each localChannels as localChannel (localChannel)}
            <button
              class:selected={selectedNodeId === local.id && selectedChannel === localChannel}
              type="button"
              onclick={() => onchoose(local.id, localChannel)}
            >
              <b>{local.name}</b>
              <small>{localChannel} · Ready to run</small>
            </button>
          {/each}
        </div>
      {:else}
        <p>
          {#if localReadiness?.canRun}
            This ready node is not a member of an open channel. Add it to one before starting work here.
          {:else}
            This node is not a task runner yet{localReadiness ? `: ${localReadiness.label.toLowerCase()}.` : '.'}
            <a href="/settings#maintenance">Prepare it</a> if you want to run work here.
          {/if}
        </p>
      {/if}
    </div>

    <div class="target-group">
      <span class="label">Use an existing setup</span>
      {#if peers.length}
        <div class="peer-list">
          {#each peers as peer (peer.node.id + ':' + peer.channel)}
            <button
              class:selected={selectedNodeId === peer.node.id && selectedChannel === peer.channel}
              type="button"
              onclick={() => onchoose(peer.node.id, peer.channel)}
            >
              <b>{peer.node.name}</b>
              <small>{peer.channel} · Ready to run</small>
            </button>
          {/each}
        </div>
      {:else}
        <p>
          No channel member is ready to run work. <a href="/settings#mesh">Connect an existing node</a>; this controller does not need a local provider to use a ready peer.
        </p>
      {/if}
    </div>
  </div>

  {#if selected}
    <div class="route">
      <h3>{selected.node.name} is selected</h3>
      <ol>
        <li>
          {#if selected.node.is_self}
            Choose or import a repository on {selected.node.name}.
          {:else}
            Enter a repository path on {selected.node.name}. Local imports are not sent to a peer.
          {/if}
        </li>
        <li>Start the task, then open its session for progress and permission requests.</li>
      </ol>
      <p><b>{channel?.name ?? 'Selected channel'}</b> · {dailyLimit}</p>
      <details class="device">
        <summary>Budgets, approvals and recovery</summary>
        <p>Set a session token cap in the composer, or leave it blank to use channel or node defaults.</p>
        <p>Policy-denied actions do not run. Approval requests wait in the session; allowing one applies only to that request.</p>
        <p><b>Stop</b> ends the harness, not its isolated workspace or records. A submitted candidate waits for review before publication. “Discard edits” clears only changes made in the review editor.</p>
      </details>
    </div>
  {/if}

  <details class="device">
    <summary>Optional device setup</summary>
    <p><a href="/settings#devices">Register this browser for notifications</a> if you want permission and review prompts here. It does not add a model or make a node eligible to run work.</p>
  </details>
</section>

<style>
  .guide {
    display: grid;
    gap: 14px;
    max-width: 760px;
    padding: 16px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--s1);
  }
  .heading {
    display: flex;
    align-items: start;
    justify-content: space-between;
    gap: 12px;
  }
  .eyebrow,
  .label {
    display: block;
    color: var(--dim);
    font: 11px var(--mono);
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  h2,
  h3,
  p,
  ol {
    margin: 0;
  }
  h2 {
    margin-top: 2px;
    font: 600 19px var(--sans);
  }
  h3 {
    font: 600 14px var(--sans);
  }
  .session {
    color: var(--acc);
    font: 12px var(--mono);
    text-align: right;
  }
  .targets {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 10px;
  }
  .target-group {
    display: grid;
    align-content: start;
    gap: 6px;
  }
  button {
    display: grid;
    gap: 2px;
    min-height: 52px;
    padding: 9px 10px;
    color: var(--ink);
    text-align: left;
    background: var(--s2);
    border: 1px solid var(--line);
    border-radius: 4px;
    cursor: pointer;
  }
  button:hover,
  button.selected {
    border-color: var(--acc);
    background: var(--s3);
  }
  button b {
    font: 600 13.5px var(--sans);
  }
  small,
  p,
  li {
    color: var(--ink2);
    font: 12.5px/1.45 var(--sans);
  }
  .peer-list {
    display: grid;
    gap: 5px;
  }
  .route {
    display: grid;
    gap: 8px;
    padding: 12px;
    border-left: 2px solid var(--acc);
    background: var(--s2);
  }
  ol {
    display: grid;
    gap: 4px;
    padding-left: 20px;
  }
  .device {
    color: var(--ink2);
    font: 12.5px/1.45 var(--sans);
  }
  .device p {
    margin-top: 6px;
  }
  @media (max-width: 640px) {
    .heading,
    .targets {
      grid-template-columns: 1fr;
      display: grid;
    }
    .session {
      text-align: left;
    }
  }
</style>
