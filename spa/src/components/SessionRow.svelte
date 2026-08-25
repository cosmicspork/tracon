<script lang="ts">
  import { formatAge, formatBudget } from '../lib/format'
  import type { Session } from '../lib/types'

  let { session }: { session: Session } = $props()

  const tone = $derived(
    session.state === 'waiting_on_you'
      ? 'wait'
      : session.state === 'failed' || session.state === 'killed_budget'
        ? 'crit'
        : session.state === 'closed'
          ? 'ok'
          : 'run',
  )
  const kind = $derived(
    {
      starting: 'Starting',
      running: session.turn_active ? 'Working' : 'Running',
      waiting_on_you: 'Waiting on you',
      waiting_on_check: 'Waiting on a check',
      closed: session.end_reason === 'killed_user' ? 'Killed' : 'Closed',
      killed_budget: 'Killed · budget',
      failed: 'Failed',
    }[session.state],
  )
  const repo = $derived(session.repo_path.split('/').at(-1) ?? session.repo_path)
</script>

<a class="row {tone}" href="/sessions/{session.id}">
  <span class="bar"></span>
  <span class="mono">{session.node_id.slice(0, 8)}</span>
  <span class="mono age">{formatAge(session.updated_ms)}</span>
  <span class="t">
    <em>{kind}</em>
    {session.branch}
    <small
      >{repo} · {session.model.split('/').at(-1)} · {session.channel}{session.last_error
        ? ` · ${session.last_error}`
        : ''}</small
    >
  </span>
  <span class="mono">{formatBudget(session.tokens_used, session.budget_tokens)}</span>
</a>

<style>
  .row {
    display: grid;
    grid-template-columns: 3px 72px 48px minmax(0, 1fr) 84px;
    gap: 0 14px;
    align-items: center;
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
    text-decoration: none;
    color: inherit;
  }
  .bar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
  }
  .row.wait .bar {
    background: var(--wait);
  }
  .row.crit .bar {
    background: var(--crit);
  }
  .row.run .bar {
    background: var(--acc);
  }
  .row.ok .bar {
    background: var(--ok);
  }
  .row.wait {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
  }
  .row.crit {
    background: linear-gradient(90deg, var(--wash-crit), var(--s1) 42%);
  }
  .row.run {
    background: linear-gradient(90deg, var(--wash-run), var(--s1) 42%);
  }
  .row.ok {
    background: linear-gradient(90deg, var(--wash-ok), var(--s1) 42%);
  }
  .t {
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
    color: var(--ink2);
  }
  .row.wait .t em {
    color: var(--wait);
  }
  .row.crit .t em {
    color: var(--crit);
  }
  .row.run .t em {
    color: var(--acc);
  }
  .row.ok .t em {
    color: var(--ok);
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    margin-top: 2px;
  }
  @media (max-width: 700px) {
    .row {
      grid-template-columns: 3px 64px minmax(0, 1fr);
    }
    .age,
    .row > .mono:last-child {
      display: none;
    }
  }
</style>
