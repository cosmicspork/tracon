// A session create carries the caller's own id for it, so that sending it
// again after an answer that never came cannot start a second session: the
// node that owns it answers a create whose id it has seen with the session
// the first one made. One id belongs to one start of one form: it is kept
// while the operator retries what the node could not answer for, and dropped
// once that start is settled or the form says something else.

/** A fresh id: a v4 UUID. `crypto.randomUUID` is only offered on a secure
    origin, and a node is often reached over plain http on the LAN. */
export function newRequestId(random: (bytes: Uint8Array) => Uint8Array = (b) => crypto.getRandomValues(b)): string {
  const b = random(new Uint8Array(16))
  b[6] = (b[6] & 0x0f) | 0x40
  b[8] = (b[8] & 0x3f) | 0x80
  const hex = Array.from(b, (x) => x.toString(16).padStart(2, '0'))
  const group = (from: number, to: number) => hex.slice(from, to).join('')
  return [group(0, 4), group(4, 6), group(6, 8), group(8, 10), group(10, 16)].join('-')
}

/** The request id one form's starts are sent with. */
export class StartRequest {
  private held: { key: string; id: string } | null = null

  constructor(private mint: () => string = () => newRequestId()) {}

  /** The id for a start of `spec`: the one kept for exactly this spec, else
      a new one. */
  idFor(spec: unknown): string {
    const key = JSON.stringify(spec)
    if (this.held?.key !== key) this.held = { key, id: this.mint() }
    return this.held.id
  }

  /** What the node said about the last start. Only an unknown outcome keeps
      the id, so pressing start again finds the session if it was made; an
      answer either way settles it. */
  settle(errorKind: string | undefined | null): void {
    if (errorKind !== 'outcome_unknown') this.held = null
  }
}
