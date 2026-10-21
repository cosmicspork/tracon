// The real lines of a commit message, for the review screen's message boxes.
// The box soft-wraps a long line to its width; the gutter numbers each real
// line once, at its first visual row, so a wrapped continuation reads apart
// from a line the message actually breaks. Nothing here touches the value.

/** Where git convention ends a subject line and wraps a body line. */
export const COMMIT_COLUMN = 72

/**
 * The message split at its real line breaks: `\n`, `\r\n` or a lone `\r`. A
 * trailing break leaves an empty last line, as an editor shows one, since
 * that is where the caret goes next.
 */
export function realLines(text: string): string[] {
  return text.split(/\r\n|\r|\n/)
}

/** Characters in a line, counting an astral character once. */
export function columns(line: string): number {
  let n = 0
  for (const _ of line) n++
  return n
}

/** The subject's length when it runs past the convention's column, else null. Shown, never enforced. */
export function longSubject(text: string, limit = COMMIT_COLUMN): number | null {
  const n = columns(realLines(text)[0])
  return n > limit ? n : null
}
