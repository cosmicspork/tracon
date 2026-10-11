// A call to one of the node's own MCP tools, read as what it does: "Wait for
// review 01a128ee", "Ask the operator · Which staging database…", "Pull
// request #439". The harnesses name these tools by their protocol, not for
// people: Claude Code calls them `mcp__tracon__review_status`, OpenCode
// `tracon_review_status`. Another MCP server's tools keep their server's name
// in view, since that is what the operator needs to know about them.

/// Every tool the node offers a session, by the name it is offered under.
/// OpenCode's `tracon_<tool>` is read as the node's only for these, so that a
/// harness tool that happens to start with `tracon_` is not taken for one.
export const NODE_TOOLS = new Set([
  // review and reports
  'submit_review',
  'review_status',
  'review_verdict',
  'submit_report',
  'report_status',
  // the operator
  'ask_operator',
  'question_status',
  'notify_operator',
  'report_issue',
  'issue_report_status',
  'approval_status',
  'request_egress',
  'show_work',
  // GitHub
  'pr_status',
  'pr_comment',
  'pr_reply',
  'pr_threads',
  'pr_for_branch',
  'pr_merge',
  'run_status',
  'run_logs',
  'run_rerun',
  'run_wait',
  // GitLab
  'mr_status',
  'mr_comment',
  'mr_reply',
  'mr_discussions',
  'mr_for_branch',
  'mr_merge',
  'pipeline_status',
  'pipeline_wait',
  'pipeline_list_by_sha',
  'pipeline_run',
  'job_trace',
  'job_play',
  'deploy',
  // Jira
  'issue',
  'issue_search',
  'issue_comment',
  'issue_update',
  'issue_create',
  'issue_transition',
  'issue_transitions',
  // documents, memory, databases
  'doc_read',
  'doc_search',
  'doc_write',
  'recall',
  'retain',
  'query',
  'describe',
  // services and repository setup
  'service_start',
  'service_status',
  'repo_setup_draft',
  'repo_setup_try',
  'repo_setup_propose',
  // the work item
  'work_ready',
  'work_discover',
  'work_close',
  'brief_read',
  'brief_note',
  'criteria_read',
  'criteria_link',
])

/// Which of the node's tools a harness's tool name is, or null when it is
/// not one of them.
export function nodeTool(title: string): string | null {
  const name = title.trim()
  const claude = name.match(/^mcp__tracon__([a-z0-9_]+)$/)
  if (claude) return claude[1]
  const opencode = name.match(/^tracon_([a-z0-9_]+)$/)
  return opencode && NODE_TOOLS.has(opencode[1]) ? opencode[1] : null
}

/// Another MCP server's tool as Claude Code names it, `mcp__<server>__<tool>`.
export function mcpTool(title: string): { server: string; tool: string } | null {
  const m = title.trim().match(/^mcp__([^_].*?)__(.+)$/)
  return m ? { server: m[1], tool: m[2] } : null
}

function str(v: unknown): string {
  return typeof v === 'string' ? v.trim() : ''
}

function num(v: unknown): string {
  return typeof v === 'number' && Number.isFinite(v) ? String(v) : str(v)
}

/// The first line of some prose, cut to fit a transcript line.
function clip(text: string, max = 72): string {
  const line = text.split('\n')[0].trim()
  return line.length > max ? `${line.slice(0, max - 1).trimEnd()}…` : line
}

/// The start of an id the node minted: enough to tell two apart on one screen.
function short(id: string): string {
  return id.length > 12 ? id.slice(0, 8) : id
}

/// "review status" for `review_status`: what an unmapped tool reads as.
export function spaced(tool: string): string {
  const words = tool.replace(/_/g, ' ').trim()
  return words.charAt(0).toUpperCase() + words.slice(1)
}

/// Parts joined by " · ", with the empty ones left out: an action and the
/// prose it carries (a title, a question).
function line(...parts: string[]): string {
  return parts.filter(Boolean).join(' · ')
}

/// Parts joined by a space: an action and the name it acts on (an id, a
/// slug, a branch), read as one phrase like "Read src/x.ts".
function sp(...parts: string[]): string {
  return parts.filter(Boolean).join(' ')
}

/// "Ask the operator", "Wait for review 01a128ee": the tool and what it was
/// called with, as a line to scan.
export function nodeCallText(tool: string, rawInput: unknown): string {
  // A number the schema requires but the call left out leaves "Comment on #";
  // the dangling mark goes rather than pointing at nothing.
  return callLine(tool, rawInput).replace(/\s+[#!]?$/, '')
}

function callLine(tool: string, rawInput: unknown): string {
  const a: Record<string, unknown> =
    rawInput && typeof rawInput === 'object' && !Array.isArray(rawInput) ? (rawInput as Record<string, unknown>) : {}
  const pr = num(a.number)
  const mr = num(a.iid)
  const run = num(a.run_id)
  const pipeline = num(a.pipeline_id)
  const job = num(a.job_id)
  const key = str(a.key)
  const many = (ids: unknown) => (Array.isArray(ids) && ids.length > 1 ? `${ids.length} of them` : '')
  switch (tool) {
    case 'submit_review':
      return line(str(a.review_id) ? 'Resubmit for review' : 'Submit for review', clip(str(a.title)))
    case 'review_status':
      return sp('Wait for review', short(str(a.review_id)))
    case 'review_verdict':
      return line('Give a verdict', str(a.verdict))
    case 'submit_report':
      return line(str(a.report_id) ? 'Revise the report' : 'Submit a report', clip(str(a.title)))
    case 'report_status':
      return sp('Wait for the report', short(str(a.report_id)))
    case 'ask_operator':
      return line('Ask the operator', clip(str(a.question)))
    case 'question_status':
      return line('Wait for an answer', many(a.question_ids))
    case 'notify_operator':
      return line('Notify the operator', clip(str(a.title) || str(a.message)))
    case 'report_issue':
      return line('Report an issue', clip(str(a.title)))
    case 'issue_report_status':
      return line('Wait for the issue report', many(a.issue_ids))
    case 'approval_status':
      return line('Wait for an approval', many(a.approval_ids))
    case 'request_egress':
      return line(`Ask to reach ${str(a.host) || 'a host'}`, clip(str(a.why), 48))
    case 'show_work':
      return line('Show work', clip(str(a.title)))
    case 'pr_status':
      return `Pull request #${pr}`
    case 'pr_comment':
      return `Comment on #${pr}`
    case 'pr_reply':
      return `Reply on #${pr}`
    case 'pr_threads':
      return `Review threads on #${pr}`
    case 'pr_for_branch':
      return sp('Pull request for', str(a.branch))
    case 'pr_merge':
      return `Merge #${pr}`
    case 'run_status':
      if (run) return `CI run ${run}`
      if (str(a.sha)) return `CI runs at ${str(a.sha).slice(0, 7)}`
      return sp('CI runs', str(a.branch) ? `on ${str(a.branch)}` : '')
    case 'run_logs':
      return `CI log of job ${job}`
    case 'run_rerun':
      return `Rerun CI run ${run}`
    case 'run_wait':
      return `Wait for CI run ${run}`
    case 'mr_status':
      return `Merge request !${mr}`
    case 'mr_comment':
      return `Comment on !${mr}`
    case 'mr_reply':
      return `Reply on !${mr}`
    case 'mr_discussions':
      return `Discussions on !${mr}`
    case 'mr_for_branch':
      return sp('Merge request for', str(a.branch))
    case 'mr_merge':
      return `Merge !${mr}`
    case 'pipeline_status':
      return pipeline ? `Pipeline ${pipeline}` : sp('Pipelines', str(a.ref) ? `on ${str(a.ref)}` : '')
    case 'pipeline_wait':
      return `Wait for pipeline ${pipeline}`
    case 'pipeline_list_by_sha':
      return `Pipelines at ${str(a.sha).slice(0, 7)}`
    case 'pipeline_run':
      return sp('Run a pipeline', str(a.ref) ? `on ${str(a.ref)}` : '')
    case 'job_trace':
      return `Log of job ${job}`
    case 'job_play':
      return `Start job ${job}`
    case 'deploy':
      return `Deploy job ${job}`
    case 'issue':
      return `Issue ${key}`
    case 'issue_search':
      return line('Search issues', clip(str(a.jql), 56))
    case 'issue_comment':
      return `Comment on ${key}`
    case 'issue_update':
      return `Update ${key}`
    case 'issue_create':
      return line('Create an issue', clip(str(a.summary)))
    case 'issue_transition':
      return `Move ${key}`
    case 'issue_transitions':
      return `Transitions of ${key}`
    case 'doc_read':
      return sp('Read document', str(a.slug))
    case 'doc_search':
      return line('Search documents', clip(str(a.query), 56))
    case 'doc_write':
      return sp('Write document', str(a.slug))
    case 'recall':
      return line('Recall', clip(str(a.query), 56))
    case 'retain':
      return line(str(a.kind) ? `Remember a ${str(a.kind)}` : 'Remember', clip(str(a.body), 56))
    case 'query':
      return line('Query', clip(str(a.sql), 56))
    case 'describe':
      return sp('Describe', str(a.table))
    case 'service_start':
      return sp('Start service', str(a.name))
    case 'service_status':
      return sp('Service', str(a.name))
    case 'repo_setup_draft':
      return "Draft the repository's setup"
    case 'repo_setup_try':
      return "Try the repository's setup"
    case 'repo_setup_propose':
      return "Propose the repository's setup"
    case 'work_ready':
      return 'Ready work'
    case 'work_discover':
      return line('Record new work', clip(str(a.title)))
    case 'work_close':
      return line('Close the work item', clip(str(a.summary), 56))
    case 'brief_read':
      return 'Read the brief'
    case 'brief_note':
      return sp('Note on the brief', str(a.field))
    case 'criteria_read':
      return 'Read the acceptance criteria'
    case 'criteria_link':
      return sp('Link evidence to', str(a.criterion))
    default:
      return spaced(tool)
  }
}

/// A bare tool name, as a repetition notice or a policy record carries it,
/// read the same way as a call's line when it is one of the node's tools or
/// another MCP server's; any other name is left as it is.
export function toolName(title: string): string {
  const node = nodeTool(title)
  if (node) return nodeCallText(node, {})
  const mcp = mcpTool(title)
  return mcp ? `${spaced(mcp.tool)} (${mcp.server})` : title
}

/// The few tools whose answer is a state worth reading without opening the
/// call: a review still checking, a question answered, a host allowed.
const STATEFUL = new Set([
  'submit_review',
  'review_status',
  'submit_report',
  'report_status',
  'ask_operator',
  'question_status',
  'issue_report_status',
  'approval_status',
  'request_egress',
  'run_wait',
  'pipeline_wait',
  'service_start',
  'service_status',
])

/// What a node tool's answer said, in a word or two, read from the result's
/// `state` (or `status`) field. A cut-off answer is read by hand, so the state
/// still shows when the node kept only the start of it.
export function nodeCallNote(tool: string, output: string): string {
  if (!STATEFUL.has(tool) || !output) return ''
  let state = ''
  try {
    const parsed: unknown = JSON.parse(output)
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      const o = parsed as Record<string, unknown>
      state = str(o.state) || str(o.status)
    }
  } catch {
    state = output.match(/"(?:state|status)"\s*:\s*"([a-z_ ]{1,32})"/)?.[1] ?? ''
  }
  return state.replace(/_/g, ' ')
}
