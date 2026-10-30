import { afterEach, describe, expect, test } from 'bun:test'

import { ApiError, call, failureText } from './api'

const cloudflare504 = `<!DOCTYPE html>
<!--[if lt IE 7]> <html class="no-js ie6 oldie" lang="en-US"> <![endif]-->
<html class="no-js" lang="en-US">
<head><title>0x69.xyz | 504: Gateway time-out</title></head>
<body><h1>Gateway time-out</h1><script>(function(){})()</script></body>
</html>`

describe('failureText', () => {
  test('names a proxy error page by its title instead of dumping its HTML', () => {
    const text = failureText(504, 'Gateway Timeout', cloudflare504)
    expect(text).toBe('the node did not answer (0x69.xyz | 504: Gateway time-out); try again in a moment')
  })

  test('falls back to the status for a page without a title', () => {
    expect(failureText(502, 'Bad Gateway', '<html><body>down</body></html>')).toBe(
      'the node did not answer (502 Bad Gateway); try again in a moment',
    )
  })

  test("keeps a plain-text refusal as the node wrote it", () => {
    expect(failureText(400, 'Bad Request', 'Failed to parse the request body as JSON')).toBe(
      'Failed to parse the request body as JSON',
    )
  })

  test('says the status for an empty body', () => {
    expect(failureText(503, 'Service Unavailable', '')).toBe('503 Service Unavailable')
  })
})

describe('call', () => {
  const realFetch = globalThis.fetch
  afterEach(() => {
    globalThis.fetch = realFetch
  })

  test('a 504 page from a proxy becomes a short error', async () => {
    globalThis.fetch = (async () =>
      new Response(cloudflare504, { status: 504, statusText: 'Gateway Timeout', headers: { 'content-type': 'text/html' } })) as unknown as typeof fetch
    const err = (await call('POST', '/api/reviews/r1/approve').catch((e: unknown) => e)) as ApiError
    expect(err).toBeInstanceOf(ApiError)
    expect(err.status).toBe(504)
    expect(err.message).not.toContain('<')
    expect(err.message).toContain('504: Gateway time-out')
  })
})
