<script lang="ts">
  import { groupLog, groupOpen, groupSummary } from '../lib/log'
  import { formatTokens } from '../lib/format'
  import type { Event } from '../lib/types'

  let {
    events,
    openChunks,
    toolProgress,
  }: {
    events: Event[]
    openChunks: Map<string, { kind: string; text: string }>
    toolProgress: Map<string, string>
  } = $props()

  const entries = $derived(groupLog(events, toolProgress))

  let el = $state<HTMLElement | null>(null)
  let pinned = $state(true)

  // Follow the stream unless the operator has scrolled up to read.
  $effect(() => {
    void entries.length
    void openChunks
    if (pinned && el) el.scrollTop = el.scrollHeight
  })

  function onScroll() {
    if (!el) return
    pinned = el.scrollHeight - el.scrollTop - el.clientHeight < 40
  }

  function text(e: Event): string {
    return typeof e.payload.text === 'string' ? e.payload.text : ''
  }

  function toolLine(call: Event, result: Event | undefined, progress: string | undefined): string {
    const title = (call.payload.title as string) ?? call.ref_id ?? 'tool'
    const status = (result?.payload.status as string) ?? progress ?? 'running'
    const truncated = result?.payload.truncated === true ? ' · output truncated' : ''
    return `${title} · ${status}${truncated}`
  }

  function turnEnd(e: Event): string {
    const usage = e.payload.usage as { total_tokens?: number } | undefined
    const total = usage?.total_tokens
    return total !== undefined
      ? `turn ended · ${formatTokens(total)} tokens`
      : `turn ended · ${e.payload.stop_reason ?? ''}`
  }
</script>

<div class="log" bind:this={el} onscroll={onScroll}>
  {#each entries as entry, i (entry.event?.seq ?? `tools-${i}`)}
    {#if entry.kind === 'leaf'}
      {@const e = entry.event!}
      {#if e.kind === 'user_prompt'}
        <div class="you">{text(e)}</div>
      {:else if e.kind === 'message'}
        <div class="msg">{text(e)}</div>
      {:else if e.kind === 'thought'}
        <details class="fold">
          <summary>thought</summary>
          <div>{text(e)}</div>
        </details>
      {:else if e.kind === 'permission_request'}
        <div class="mark wait">permission · {e.payload.title}</div>
      {:else if e.kind === 'permission_answer'}
        <div class="mark">answered: {e.payload.option_id}</div>
      {:else if e.kind === 'permission_expired'}
        <div class="mark crit">{e.payload.reason ?? 'denied: unanswered'}</div>
      {:else if e.kind === 'turn_end'}
        <div class="mark">{turnEnd(e)}</div>
      {:else if e.kind === 'worktree'}
        <div class="sys">
          worktree {e.payload.path} on {e.payload.branch} from {e.payload.base}{e.payload
            .main_checkout_dirty
            ? ' · main checkout is dirty and was left alone'
            : ''}
        </div>
      {:else if e.kind === 'session_started'}
        <div class="sys">harness started · {e.payload.model}</div>
      {:else if e.kind === 'state'}
        <div class="sys">→ {e.payload.state}</div>
      {:else if e.kind === 'error'}
        <div class="mark crit">{e.payload.error ?? JSON.stringify(e.payload)}</div>
      {:else if e.kind === 'tool_result'}
        <div class="sys">{toolLine(e, e, undefined)}</div>
      {:else if e.kind !== 'usage' && e.kind !== 'plan'}
        <div class="sys">{e.kind}</div>
      {/if}
    {:else}
      {@const tools = entry.tools!}
      {@const open = groupOpen(tools)}
      <details class="fold tools" open={open || tools.length === 1}>
        <summary>{groupSummary(tools)}</summary>
        <div>
          {#each tools as t (t.call.seq)}
            <div class="tool" class:crit={t.result?.payload.status === 'failed'}>
              {toolLine(t.call, t.result, t.progress)}
            </div>
          {/each}
        </div>
      </details>
    {/if}
  {/each}
  {#each [...openChunks.values()] as chunk, i (i)}
    {#if chunk.kind === 'message'}
      <div class="msg live">{chunk.text}<span class="cursor">▍</span></div>
    {:else}
      <div class="sys live">{chunk.text}</div>
    {/if}
  {/each}
</div>

<style>
  .log {
    font: 12.5px/1.6 var(--mono);
    display: flex;
    flex-direction: column;
    gap: 6px;
    background: var(--s1);
    border-radius: 4px;
    padding: 14px 16px;
    overflow-y: auto;
    flex: 1;
    min-height: 200px;
  }
  .you {
    color: var(--acc);
  }
  .you::before {
    content: '› ';
  }
  .msg {
    color: var(--ink);
    white-space: pre-wrap;
  }
  .sys {
    color: var(--dim);
  }
  .mark {
    color: var(--ink2);
  }
  .mark.wait {
    color: var(--wait);
  }
  .mark.crit,
  .tool.crit {
    color: var(--crit);
  }
  .fold summary {
    color: var(--dim);
    font-size: 12px;
    cursor: pointer;
    list-style: none;
  }
  .fold summary::-webkit-details-marker {
    display: none;
  }
  .fold summary::before {
    content: '▸';
    display: inline-block;
    width: 12px;
  }
  .fold[open] summary::before {
    content: '▾';
  }
  .fold[open] summary {
    color: var(--ink2);
  }
  .fold > div {
    padding: 4px 0 2px 18px;
    border-left: 1px solid var(--rule);
    margin: 4px 0 0 4px;
    color: var(--dim);
    white-space: pre-wrap;
  }
  .cursor {
    color: var(--ink2);
    animation: blink 1s steps(1) infinite;
  }
  @keyframes blink {
    50% {
      opacity: 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .cursor {
      animation: none;
    }
  }
</style>
