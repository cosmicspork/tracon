import { expect, test } from 'bun:test'

import { mcpTool, NODE_TOOLS, nodeCallNote, nodeCallText, nodeTool, toolName } from './nodetools'

test("the node's tools are known by either harness's name for them", () => {
  expect(nodeTool('mcp__tracon__review_status')).toBe('review_status')
  expect(nodeTool('tracon_review_status')).toBe('review_status')
  // A harness tool that only starts like one is not the node's.
  expect(nodeTool('tracon_session')).toBeNull()
  expect(nodeTool('mcp__forge__comment')).toBeNull()
  expect(nodeTool('Bash')).toBeNull()
  expect(mcpTool('mcp__plugin_playwright_playwright__browser_click')).toEqual({
    server: 'plugin_playwright_playwright',
    tool: 'browser_click',
  })
  expect(mcpTool('Bash')).toBeNull()
})

test('review and report calls name what they wait on', () => {
  expect(nodeCallText('submit_review', { title: 'fix(spa): read the worktree line' })).toBe(
    'Submit for review · fix(spa): read the worktree line',
  )
  expect(nodeCallText('submit_review', { title: 'fix: x', review_id: '01a128ee-0940' })).toBe('Resubmit for review · fix: x')
  expect(nodeCallText('review_status', { review_id: '01a128ee-0940-7cf0-85e7-a5f3aa718404' })).toBe('Wait for review 01a128ee')
  expect(nodeCallText('review_verdict', { verdict: 'approve', summary: 'ok' })).toBe('Give a verdict · approve')
  expect(nodeCallText('submit_report', { title: 'Why exports double' })).toBe('Submit a report · Why exports double')
  expect(nodeCallText('report_status', { report_id: 'r-1' })).toBe('Wait for the report r-1')
})

test('operator calls carry the question, the host or the title', () => {
  const long = 'In-session ask_operator test: answer anything once a minute has passed, and then a good deal more.'
  const asked = nodeCallText('ask_operator', { request_id: 'x', question: long })
  expect(asked.startsWith('Ask the operator · In-session ask_operator test')).toBe(true)
  expect(asked.endsWith('…')).toBe(true)
  expect(nodeCallText('ask_operator', { question: 'First line\nsecond line' })).toBe('Ask the operator · First line')
  expect(nodeCallText('question_status', { question_id: 'q' })).toBe('Wait for an answer')
  expect(nodeCallText('question_status', { question_ids: ['a', 'b', 'c'] })).toBe('Wait for an answer · 3 of them')
  expect(nodeCallText('request_egress', { host: 'pypi.org', why: 'uv sync' })).toBe('Ask to reach pypi.org · uv sync')
  expect(nodeCallText('notify_operator', { title: 'Checks pass', message: 'm' })).toBe('Notify the operator · Checks pass')
  expect(nodeCallText('report_issue', { title: 'review_status lags' })).toBe('Report an issue · review_status lags')
  expect(nodeCallText('approval_status', { approval_id: 'a' })).toBe('Wait for an approval')
  expect(nodeCallText('show_work', { title: 'Before and after' })).toBe('Show work · Before and after')
})

test('forge calls name the change, run or pipeline', () => {
  expect(nodeCallText('pr_status', { number: 439 })).toBe('Pull request #439')
  expect(nodeCallText('pr_comment', { number: 439, body: 'b' })).toBe('Comment on #439')
  expect(nodeCallText('pr_for_branch', { branch: 'fix/x' })).toBe('Pull request for fix/x')
  expect(nodeCallText('pr_merge', { number: 12, head_sha: 'abc', operation_id: 'o' })).toBe('Merge #12')
  expect(nodeCallText('run_status', { run_id: 37854905007 })).toBe('CI run 37854905007')
  expect(nodeCallText('run_status', { sha: '5fafbe54b75e482c' })).toBe('CI runs at 5fafbe5')
  expect(nodeCallText('run_status', { branch: 'main' })).toBe('CI runs on main')
  expect(nodeCallText('run_logs', { job_id: 9 })).toBe('CI log of job 9')
  expect(nodeCallText('run_wait', { run_id: 5 })).toBe('Wait for CI run 5')
  expect(nodeCallText('mr_status', { project: 'g/p', iid: 7 })).toBe('Merge request !7')
  expect(nodeCallText('pipeline_wait', { pipeline_id: 88 })).toBe('Wait for pipeline 88')
  expect(nodeCallText('issue', { key: 'OPS-12' })).toBe('Issue OPS-12')
  expect(nodeCallText('issue_transition', { key: 'OPS-12', transition_id: '21' })).toBe('Move OPS-12')
  // A number the call left out leaves no dangling mark.
  expect(nodeCallText('pr_comment', {})).toBe('Comment on')
})

test('documents, memory, services, setup and the work item read as actions', () => {
  expect(nodeCallText('doc_read', { slug: 'note-tracon-dogfood' })).toBe('Read document note-tracon-dogfood')
  expect(nodeCallText('doc_search', { query: 'dogfood' })).toBe('Search documents · dogfood')
  expect(nodeCallText('recall', { query: 'gateway' })).toBe('Recall · gateway')
  expect(nodeCallText('retain', { kind: 'lesson', body: 'x' })).toBe('Remember a lesson · x')
  expect(nodeCallText('query', { sql: 'SELECT 1' })).toBe('Query · SELECT 1')
  expect(nodeCallText('service_start', { name: 'browser' })).toBe('Start service browser')
  expect(nodeCallText('repo_setup_draft', {})).toBe("Draft the repository's setup")
  expect(nodeCallText('brief_read', {})).toBe('Read the brief')
  expect(nodeCallText('criteria_link', { criterion: 'c1', kind: 'test', value: 'v' })).toBe('Link evidence to c1')
  expect(nodeCallText('work_close', { summary: 'done' })).toBe('Close the work item · done')
})

test('every tool the node offers reads as words, and an unknown one falls back to its name', () => {
  for (const tool of NODE_TOOLS) {
    const text = nodeCallText(tool, {})
    expect(text).not.toContain('_')
    expect(text.length).toBeGreaterThan(0)
  }
  expect(nodeCallText('frobnicate_widgets', {})).toBe('Frobnicate widgets')
  expect(nodeCallText('review_status', 'not an object')).toBe('Wait for review')
  expect(toolName('mcp__tracon__review_status')).toBe('Wait for review')
  expect(toolName('mcp__probe__ping_it')).toBe('Ping it (probe)')
  expect(toolName('Bash')).toBe('Bash')
})

test("a waiting tool's state comes out of its answer, even a cut-off one", () => {
  const answer = JSON.stringify({ state: 'checking', review_id: 'r', message: 'Still running' }, null, 2)
  expect(nodeCallNote('review_status', answer)).toBe('checking')
  expect(nodeCallNote('question_status', '{\n  "answer": {"text": "yes"},\n  "state": "answered"\n}')).toBe('answered')
  expect(nodeCallNote('review_status', '{\n  "message": "Still running",\n  "state": "still_waiting", "revi')).toBe('still waiting')
  // A tool whose answer is not a state says nothing.
  expect(nodeCallNote('doc_read', answer)).toBe('')
  expect(nodeCallNote('review_status', 'not json')).toBe('')
  expect(nodeCallNote('review_status', '')).toBe('')
})
