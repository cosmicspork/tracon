// Shown work (`show_work`) as the operator reads it. A proposal is shown the
// same way, and labelled as what it is wherever it appears: layouts or flows
// put to the operator to choose from, not an account of work done.
import type { ShownWork } from './types'

/** What an item is, as its tag reads. */
export function formatTag(item: Pick<ShownWork, 'format'>): string {
  return item.format === 'markdown' ? 'account' : item.format === 'html' ? 'page' : 'files'
}

/**
 * Accounts of work done and proposals, each in the order they were shown, so
 * they are read under headings of their own and never mistaken for each other.
 * A node from before proposals sends none.
 */
export function splitShown(items: ShownWork[]): { work: ShownWork[]; proposals: ShownWork[] } {
  return {
    work: items.filter((item) => !item.proposal),
    proposals: items.filter((item) => item.proposal),
  }
}

/** The session log's line for a `work_shown` event. */
export function shownEventLine(p: Record<string, unknown>): string {
  const files = typeof p.files === 'number' && p.files > 0 ? p.files : 0
  const head = String(p.head_sha ?? '').slice(0, 12)
  return [
    p.proposal === true ? 'proposal shown' : 'work shown',
    String(p.title ?? ''),
    `at ${head}`,
    ...(files ? [`${files} ${files === 1 ? 'file' : 'files'}`] : []),
    'in tracon only',
  ].join(' · ')
}
