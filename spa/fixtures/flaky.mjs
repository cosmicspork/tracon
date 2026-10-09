// In-page helpers for audit states that need a request to fail on one call
// and not another, or a stream frame to arrive after load. They run in the
// page (a state's `init`, `act`), so each shot counts its own calls.

/** An init script: the GETs of `path` numbered in `failing` (1-based) answer 500. */
export const flaky = (path, failing) => `{
  const real = window.fetch
  const failing = ${JSON.stringify(failing)}
  let calls = 0
  window.fetch = (input, init) => {
    const url = new URL(typeof input === 'string' ? input : input.url, location.href)
    if (url.pathname === ${JSON.stringify(path)} && failing.includes(++calls)) {
      const body = JSON.stringify({ error: { code: 500, message: 'database is locked' } })
      return Promise.resolve(new Response(body, { status: 500, headers: { 'content-type': 'application/json' } }))
    }
    return real(input, init)
  }
  const Stream = window.EventSource
  window.EventSource = class extends Stream {
    constructor(...args) {
      super(...args)
      window.__auditStream = this
    }
  }
}`

/** Deliver a stream event to the page, as the node would: `open` refetches, a named frame carries `data`. */
export const streamEvent = (page, name, data) =>
  page.evaluate(
    ([n, d]) => window.__auditStream.dispatchEvent(d === null ? new Event(n) : new MessageEvent(n, { data: JSON.stringify(d) })),
    [name, data ?? null],
  )
