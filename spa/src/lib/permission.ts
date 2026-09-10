import type { Permission } from './types'

export interface EditableField {
  key: string
  value: string
}

function toolArguments(p: Pick<Permission, 'kind' | 'raw_input'>): Record<string, unknown> | null {
  if (p.kind !== 'tool' || !p.raw_input) return null
  try {
    const args = JSON.parse(p.raw_input)?.arguments
    return args && typeof args === 'object' && !Array.isArray(args) ? args : null
  } catch {
    return null
  }
}

// Only the prose arguments of a brokered call are offered for editing; keys,
// slugs and ids stay as the agent sent them and show in the full request.
export function editableFields(p: Pick<Permission, 'kind' | 'raw_input'>): EditableField[] {
  const args = toolArguments(p)
  if (!args) return []
  return Object.entries(args)
    .filter((e): e is [string, string] => typeof e[1] === 'string' && (e[1].length > 40 || e[1].includes('\n')))
    .map(([key, value]) => ({ key, value }))
}

export function editedArguments(
  p: Pick<Permission, 'kind' | 'raw_input'>,
  drafts: Record<string, string>,
): Record<string, unknown> | undefined {
  const args = toolArguments(p)
  if (!args) return undefined
  const changed = Object.entries(drafts).filter(([k, v]) => args[k] !== v)
  return changed.length ? { ...args, ...Object.fromEntries(changed) } : undefined
}
