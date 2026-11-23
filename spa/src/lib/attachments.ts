// Files sent with a prompt: a screenshot, a log. The node checks them again
// and is the one that decides (it reads the bytes, not the name or the type a
// browser guessed); these checks say no before a large upload is made, with
// the same reasons, and never drop a file without saying so.

/** The node's limits (`session::attachments`): files per prompt, bytes per file, bytes in all. */
export const MAX_ATTACHMENTS = 8
export const MAX_ATTACHMENT_BYTES = 10 * 1024 * 1024
export const MAX_ATTACHMENTS_TOTAL = 20 * 1024 * 1024

/** What the file picker offers: images, PDF, and text a log or a data file is. */
export const ATTACH_ACCEPT =
  'image/png,image/jpeg,image/gif,image/webp,application/pdf,text/*,.log,.txt,.json,.jsonl,.ndjson,.md,.csv,.xml,.yaml,.yml,.diff,.patch'

export const ACCEPTED_TEXT = 'PNG, JPEG, GIF or WebP images, PDF, or text (a log, JSON, CSV, Markdown)'

/** What the node is sent for one file. */
export interface AttachmentUpload {
  name: string
  type: string
  /** Its bytes, base64. */
  data: string
}

/** What the node staged of one file, as its answer and the session's log say. */
export interface StagedAttachment {
  name: string
  path: string
  type: string
  size: number
  sha256: string
}

/** The little of a `File` these checks read, so they can be tested without one. */
export interface Attachable {
  name: string
  size: number
  type: string
}

const TEXT_EXT = /\.(log|txt|json|jsonl|ndjson|md|markdown|csv|xml|ya?ml|diff|patch|out|err|trace|ini|toml|conf|cfg|sql|sh|py|rs|ts|js|go|java|c|h|cpp|html?|css)$/i
const TYPES = new Set(['image/png', 'image/jpeg', 'image/gif', 'image/webp', 'application/pdf'])

/** Whether a file looks like something the node stages. A file with no type and a
 *  name that says nothing is let through: the node reads its bytes and says. */
export function looksAccepted(file: Attachable): boolean {
  const type = file.type.toLowerCase()
  if (TYPES.has(type) || type.startsWith('text/')) return true
  if (['application/json', 'application/x-ndjson', 'application/xml', 'application/yaml'].includes(type)) return true
  if (TEXT_EXT.test(file.name)) return true
  return type === '' && !/\.[a-z0-9]+$/i.test(file.name)
}

/** A size as the node says one. */
export function sizeText(bytes: number): string {
  if (bytes < 1024) return `${bytes} bytes`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`
}

/** Add `added` to what is already attached: what fits, and why each of the rest
 *  does not. Nothing is left out without a reason. */
export function addAttachments<T extends Attachable>(current: T[], added: T[]): { files: T[]; refused: string[] } {
  const files = [...current]
  const refused: string[] = []
  let total = files.reduce((sum, f) => sum + f.size, 0)
  for (const file of added) {
    const name = file.name || 'an attachment'
    if (files.length >= MAX_ATTACHMENTS) {
      refused.push(`${name}: a prompt carries at most ${MAX_ATTACHMENTS} files`)
    } else if (file.size > MAX_ATTACHMENT_BYTES) {
      refused.push(`${name} is ${sizeText(file.size)}; one attachment may be at most ${sizeText(MAX_ATTACHMENT_BYTES)}`)
    } else if (total + file.size > MAX_ATTACHMENTS_TOTAL) {
      refused.push(`${name}: the attachments would add up to more than ${sizeText(MAX_ATTACHMENTS_TOTAL)}`)
    } else if (!looksAccepted(file)) {
      refused.push(`${name} is not a type the node stages; it takes ${ACCEPTED_TEXT}`)
    } else {
      files.push(file)
      total += file.size
    }
  }
  return { files, refused }
}

/** A pasted image has no useful name ("image.png" every time): give it one that
 *  says when it was pasted, so two pasted screenshots differ. */
export function pastedName(type: string, now: Date): string {
  const ext = type.split('/')[1]?.replace('jpeg', 'jpg') || 'png'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `pasted-${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}-${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}.${ext}`
}

/** Bytes as base64, in pieces so a large file does not overflow the call stack. */
export function toBase64(bytes: Uint8Array): string {
  let binary = ''
  const step = 0x8000
  for (let i = 0; i < bytes.length; i += step) binary += String.fromCharCode(...bytes.subarray(i, i + step))
  return btoa(binary)
}

/** What the node is sent for a file. */
export async function toUpload(file: File): Promise<AttachmentUpload> {
  return { name: file.name, type: file.type, data: toBase64(new Uint8Array(await file.arrayBuffer())) }
}

/** The staged files an `attachments_staged` event names, as far as it names them. */
export function stagedOf(payload: Record<string, unknown>): StagedAttachment[] {
  const list = Array.isArray(payload.attachments) ? payload.attachments : []
  return list.filter((a): a is StagedAttachment => typeof a === 'object' && a !== null && typeof (a as StagedAttachment).name === 'string')
}
