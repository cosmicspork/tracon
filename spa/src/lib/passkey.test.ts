import { afterEach, describe, expect, test } from 'bun:test'

import { ApiError, call, setStepUpHandler, STEP_UP_HEADER, type StepUp } from './api'
import {
  ceremonyProblem,
  confirming,
  creationOptions,
  deviceLabel,
  enrollmentLive,
  fromBase64url,
  PasskeyCeremonyError,
  requestOptions,
  toBase64url,
  CEREMONY_TIMEOUT_MS,
} from './passkey'

describe('base64url', () => {
  test('round-trips every byte value without padding or unsafe characters', () => {
    const bytes = new Uint8Array(256).map((_, i) => i)
    const s = toBase64url(bytes)
    expect(s).not.toMatch(/[+/=]/)
    expect(Array.from(fromBase64url(s))).toEqual(Array.from(bytes))
  })

  test('reads what the node sends, padded or not', () => {
    expect(Array.from(fromBase64url('AQID'))).toEqual([1, 2, 3])
    expect(Array.from(fromBase64url('AQ'))).toEqual([1])
    expect(Array.from(fromBase64url('AQ=='))).toEqual([1])
    expect(Array.from(fromBase64url('-_8'))).toEqual([0xfb, 0xff])
  })
})

describe('the node’s options', () => {
  test('a registration keeps the node’s choices and decodes its binary fields', () => {
    const o = creationOptions({
      publicKey: {
        challenge: 'AQID',
        rp: { id: 'tracon.example.org', name: 'tracon' },
        user: { id: 'BAUG', name: 'operator', displayName: 'tracon operator' },
        pubKeyCredParams: [{ type: 'public-key', alg: -7 }],
        timeout: 120_000,
        excludeCredentials: [{ type: 'public-key', id: 'Bwg' }],
        authenticatorSelection: { residentKey: 'preferred', requireResidentKey: false, userVerification: 'required' },
        attestation: 'none',
        extensions: { credProps: true },
      },
    })
    expect(Array.from(new Uint8Array(o.challenge as ArrayBuffer))).toEqual([1, 2, 3])
    expect(Array.from(new Uint8Array(o.user.id as ArrayBuffer))).toEqual([4, 5, 6])
    expect(o.excludeCredentials?.length).toBe(1)
    expect(o.authenticatorSelection?.userVerification).toBe('required')
    // Never pinned to the platform: a security key or a phone across the
    // room may make one too.
    expect(o.authenticatorSelection?.authenticatorAttachment).toBeUndefined()
    expect(o.attestation).toBe('none')
    // Held to the client's own limit, for iOS PWAs that hang.
    expect(o.timeout).toBe(CEREMONY_TIMEOUT_MS)
  })

  test('an assertion requires user verification even when the node omits it', () => {
    const o = requestOptions({ publicKey: { challenge: 'AQID', rpId: 'tracon.example.org' } })
    expect(o.userVerification).toBe('required')
    expect(o.rpId).toBe('tracon.example.org')
    expect(o.allowCredentials).toEqual([])
    expect(o.timeout).toBe(CEREMONY_TIMEOUT_MS)
  })
})

describe('what a failed ceremony says', () => {
  test('a closed sheet keeps the browser’s own account', () => {
    const err = new PasskeyCeremonyError(new DOMException('The operation timed out.', 'NotAllowedError'))
    expect(err.detail).toBe('NotAllowedError: The operation timed out.')
    expect(ceremonyProblem(err)).toContain('NotAllowedError: The operation timed out.')
    expect(new PasskeyCeremonyError(null).detail).toBe('no credential returned')
  })

  test('named failures read as what they mean', () => {
    expect(ceremonyProblem(new DOMException('', 'InvalidStateError'))).toContain('already holds a passkey')
    expect(ceremonyProblem(new DOMException('rp id', 'SecurityError'))).toContain('not the one')
    expect(ceremonyProblem(new ApiError(400, 'the passkey did not verify'))).toBe('the passkey did not verify')
  })
})

describe('enrollment helpers', () => {
  test('a held registration is reused only while it has time left', () => {
    const e = { challenge_id: 'c', options: {} as never, expires_ms: 100_000 }
    expect(enrollmentLive(e, 50_000)).toBe(true)
    expect(enrollmentLive(e, 95_000)).toBe(false)
    expect(enrollmentLive(null, 0)).toBe(false)
  })

  test('a device names its passkey', () => {
    expect(deviceLabel('Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)')).toBe('iPhone')
    expect(deviceLabel('Mozilla/5.0 (Linux; Android 15; Pixel 9)')).toBe('Android')
    expect(deviceLabel('Mozilla/5.0 (Macintosh; Intel Mac OS X 15_0)')).toBe('Mac')
    expect(deviceLabel('curl/8')).toBe('Passkey')
  })
})

describe('an admin action the node wants confirmed', () => {
  const realFetch = globalThis.fetch
  afterEach(() => {
    globalThis.fetch = realFetch
    setStepUpHandler(null)
  })

  const stepUp: StepUp = {
    op_hash: 'a'.repeat(64),
    challenge_id: 'c1',
    options: { publicKey: { challenge: 'AQID', rpId: 'tracon.example.org' } },
    expires_ms: Date.now() + 120_000,
    action: 'POST /api/authority/grants',
  }
  const refusal = () =>
    new Response(
      JSON.stringify({
        error: { code: 403, kind: 'step_up_required', message: 'confirm this with your passkey' },
        step_up: stepUp,
      }),
      { status: 403 },
    )

  function recordFetch(responses: Response[]) {
    const seen: { headers: Record<string, string>; body: string | undefined }[] = []
    globalThis.fetch = (async (_path: string, init?: RequestInit) => {
      seen.push({
        headers: (init?.headers as Record<string, string>) ?? {},
        body: init?.body as string | undefined,
      })
      return responses.shift()!
    }) as typeof fetch
    return seen
  }

  test('is confirmed, then sent again once with the proof and the same bytes', async () => {
    const seen = recordFetch([refusal(), new Response(JSON.stringify({ id: 'g1' }), { status: 200 })])
    const asked: StepUp[] = []
    setStepUpHandler(async (s) => {
      asked.push(s)
      return 'the-proof'
    })
    const out = await call<{ id: string }>('POST', '/api/authority/grants', { b: 2, a: 1 })
    expect(out.id).toBe('g1')
    expect(asked).toEqual([stepUp])
    expect(seen.length).toBe(2)
    expect(seen[0].headers[STEP_UP_HEADER]).toBeUndefined()
    expect(seen[1].headers[STEP_UP_HEADER]).toBe('the-proof')
    expect(seen[1].body).toBe(seen[0].body)
  })

  test('is not prompted for twice: a second refusal is an error', async () => {
    recordFetch([refusal(), refusal()])
    let prompts = 0
    setStepUpHandler(async () => {
      prompts++
      return 'p'
    })
    const err = await call('POST', '/api/authority/grants', {}).catch((e) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect((err as ApiError).kind).toBe('step_up_required')
    expect(prompts).toBe(1)
  })

  test('cancelled, it is not sent again', async () => {
    const seen = recordFetch([refusal()])
    setStepUpHandler(async () => {
      throw new Error('Not confirmed')
    })
    const err = await call('DELETE', '/api/auth/token').catch((e) => e)
    expect((err as Error).message).toBe('Not confirmed')
    expect(seen.length).toBe(1)
  })

  test('one that cannot be confirmed says why', async () => {
    recordFetch([
      new Response(
        JSON.stringify({
          error: { code: 403, kind: 'step_up_unavailable', reason: 'no_public_origin', message: 'set public_origin' },
        }),
        { status: 403 },
      ),
    ])
    setStepUpHandler(async () => 'never')
    const err = (await call('POST', '/api/authority/grants', {}).catch((e) => e)) as ApiError
    expect(err.kind).toBe('step_up_unavailable')
    expect(err.reason).toBe('no_public_origin')
    expect(err.message).toBe('set public_origin')
  })
})

describe('what the prompt says is being confirmed', () => {
  test('the node’s line leads, the request beneath it', () => {
    expect(confirming({ summary: 'Allow repository setup for /w/x', action: 'POST /api/permissions/a1/answer' })).toEqual({
      headline: 'Allow repository setup for /w/x',
      detail: 'POST /api/permissions/a1/answer',
    })
  })
  test('without a line, the request alone', () => {
    expect(confirming({ action: 'POST /api/x' })).toEqual({ headline: 'POST /api/x', detail: null })
    expect(confirming({ summary: '  ', action: 'POST /api/x' })).toEqual({ headline: 'POST /api/x', detail: null })
  })
})
