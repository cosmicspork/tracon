import { afterEach, describe, expect, test } from 'bun:test'

import { api, ApiError, call, setStepUpHandler, STEP_UP_HEADER, type StepUp } from './api'
import {
  approvalFromHash,
  approvalHeadline,
  cannotApprove,
  confirmsElsewhere,
  DeviceCancelled,
  DeviceDenied,
  DeviceExpired,
  handOver,
  pollForApproval,
  settled,
  SLOW_DOWN_STEP_MS,
  type DeviceProof,
  type DeviceStart,
} from './device'

const refusedWith = (kind: string, status = 400) => new ApiError(status, kind, undefined, undefined, kind)

const start: DeviceStart = {
  approval_id: 'a'.repeat(22),
  kind: 'step-up',
  device_code: 'the-secret',
  user_code: 'BCDF-GHJK',
  approve_url: 'https://tracon.example.org/#approve=' + 'a'.repeat(22),
  interval: 5,
  expires_in: 600,
  expires_ms: 1_000_000,
}

/** A clock and a sleep that only record: the poll loop's waits, no real time. */
function fakeTime() {
  let t = 0
  const waits: number[] = []
  return {
    waits,
    now: () => t,
    sleep: async (ms: number) => {
      waits.push(ms)
      t += ms
    },
  }
}

describe('the approval address', () => {
  test('names the approval in its fragment and nothing else', () => {
    expect(approvalFromHash('#approve=' + 'a'.repeat(22))).toBe('a'.repeat(22))
    expect(approvalFromHash('#approve=abcDEF_-0123456789')).toBe('abcDEF_-0123456789')
    for (const hash of ['', '#token=trc1.x', '#approve=', '#approve=short', '#approve=../../x', `#approve=${'a'.repeat(22)}&x=1`]) {
      expect(approvalFromHash(hash)).toBeNull()
    }
  })
})

describe('who confirms in another browser', () => {
  test('the desktop app always, and a browser only when it has no passkeys', () => {
    expect(confirmsElsewhere({ tauri: true, passkeys: true })).toBe(true)
    expect(confirmsElsewhere({ tauri: true, passkeys: false })).toBe(true)
    expect(confirmsElsewhere({ tauri: false, passkeys: false })).toBe(true)
    expect(confirmsElsewhere({ tauri: false, passkeys: true })).toBe(false)
  })
})

describe('polling for the outcome', () => {
  test('waits its interval, slows down when told, and returns what was collected', async () => {
    const time = fakeTime()
    const answers: (Error | string)[] = [
      refusedWith('authorization_pending'),
      refusedWith('slow_down'),
      refusedWith('authorization_pending'),
      'collected',
    ]
    const codes: string[] = []
    const out = await pollForApproval(
      start,
      async (code) => {
        codes.push(code)
        const next = answers.shift()!
        if (next instanceof Error) throw next
        return next
      },
      time,
    )
    expect(out).toBe('collected')
    expect(codes).toEqual(['the-secret', 'the-secret', 'the-secret', 'the-secret'])
    // Five seconds, then five more for good after the slow_down.
    expect(time.waits).toEqual([5000, 5000, 5000 + SLOW_DOWN_STEP_MS, 5000 + SLOW_DOWN_STEP_MS])
  })

  test('a denial and an expiry end it, saying which', async () => {
    const time = fakeTime()
    const denied = await pollForApproval(
      start,
      async () => {
        throw refusedWith('access_denied')
      },
      time,
    ).catch((e) => e)
    expect(denied).toBeInstanceOf(DeviceDenied)
    for (const kind of ['expired_token', 'invalid_grant']) {
      const err = await pollForApproval(
        start,
        async () => {
          throw refusedWith(kind)
        },
        fakeTime(),
      ).catch((e) => e)
      expect(err).toBeInstanceOf(DeviceExpired)
    }
  })

  test('a dropped connection is retried until the request would have expired', async () => {
    const time = fakeTime()
    let tries = 0
    const err = await pollForApproval(
      { ...start, expires_ms: 20_000 },
      async () => {
        tries++
        throw new TypeError('Failed to fetch')
      },
      time,
    ).catch((e) => e)
    expect(err).toBeInstanceOf(DeviceExpired)
    expect(tries).toBe(4)
  })

  test('rate limited, it backs off rather than giving up', async () => {
    const time = fakeTime()
    const answers: (Error | string)[] = [refusedWith('too many polls', 429), 'ok']
    await pollForApproval(
      start,
      async () => {
        const next = answers.shift()!
        if (next instanceof Error) throw next
        return next
      },
      time,
    )
    expect(time.waits).toEqual([5000, 5000 + SLOW_DOWN_STEP_MS])
  })

  test('cancelled, it stops without another poll', async () => {
    const abort = new AbortController()
    let polls = 0
    const pending = pollForApproval(
      start,
      async () => {
        polls++
        throw refusedWith('authorization_pending')
      },
      { signal: abort.signal, sleep: (ms, signal) => new Promise((resolve, reject) => {
        const t = setTimeout(resolve, ms > 0 ? 1 : 0)
        signal?.addEventListener('abort', () => {
          clearTimeout(t)
          reject(new DeviceCancelled())
        })
      }) },
    )
    abort.abort()
    expect(await pending.catch((e) => e)).toBeInstanceOf(DeviceCancelled)
    expect(polls).toBe(0)
  })
})

describe('handing a challenge to a device approval', () => {
  const stepUp: StepUp = {
    op_hash: 'b'.repeat(64),
    challenge_id: 'c1',
    options: { publicKey: { challenge: 'AQID' } },
    expires_ms: 200_000,
    action: 'DELETE /api/passkeys/k1',
    summary: 'Remove a passkey',
  }

  test('uses the refusal’s own challenge while it is live', async () => {
    const asked: string[] = []
    const out = await handOver(
      stepUp,
      {
        assertOptions: async () => {
          throw new Error('not needed')
        },
        deviceStepUp: async (c) => {
          asked.push(c)
          return start
        },
      },
      100_000,
    )
    expect(out).toBe(start)
    expect(asked).toEqual(['c1'])
  })

  test('asks for a fresh challenge for the same operation when it expired or is gone', async () => {
    const fresh: string[] = []
    const asked: string[] = []
    const calls = {
      assertOptions: async (op: string) => {
        fresh.push(op)
        return { challenge_id: `fresh-${fresh.length}`, expires_ms: 999_999 }
      },
      deviceStepUp: async (c: string) => {
        asked.push(c)
        if (c === 'c1') throw new ApiError(410, 'this confirmation expired or was already used')
        return start
      },
    }
    // Near its expiry: replaced before it is used.
    await handOver(stepUp, calls, 195_000)
    expect(fresh).toEqual([stepUp.op_hash])
    expect(asked).toEqual(['fresh-1'])
    // The node says it is gone: replaced once.
    await handOver(stepUp, calls, 100_000)
    expect(asked).toEqual(['fresh-1', 'c1', 'fresh-2'])
    // Anything else is the error it is.
    const err = await handOver(
      stepUp,
      { ...calls, deviceStepUp: async () => Promise.reject(new ApiError(409, 'at the node')) },
      100_000,
    ).catch((e) => e)
    expect((err as ApiError).status).toBe(409)
  })
})

describe('an admin action from the desktop app', () => {
  const realFetch = globalThis.fetch
  afterEach(() => {
    globalThis.fetch = realFetch
    setStepUpHandler(null)
  })

  test('is confirmed in another browser, then sent again once with the proof', async () => {
    const stepUp: StepUp = {
      op_hash: 'c'.repeat(64),
      challenge_id: 'c1',
      options: { publicKey: { challenge: 'AQID' } },
      expires_ms: Date.now() + 120_000,
      action: 'POST /api/authority/grants',
      summary: 'Create an authority grant',
    }
    const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
    const responses = [
      json({ error: { code: 403, kind: 'step_up_required', message: 'confirm' }, step_up: stepUp }, 403),
      json({ ...start, op_hash: stepUp.op_hash }),
      json({ error: { code: 400, kind: 'authorization_pending', message: 'waiting' } }, 400),
      json({ kind: 'step-up', proof: 'the-proof', op_hash: stepUp.op_hash, expires_ms: 1 }),
      json({ id: 'g1' }),
    ]
    const seen: { path: string; headers: Record<string, string>; body?: string }[] = []
    globalThis.fetch = (async (path: string, init?: RequestInit) => {
      seen.push({ path, headers: (init?.headers as Record<string, string>) ?? {}, body: init?.body as string })
      return responses.shift()!
    }) as typeof fetch

    // What the prompt does when this window cannot make a passkey.
    setStepUpHandler(async (s) => {
      const started = await handOver(s, {
        assertOptions: (op) => api.passkeyAssertOptions(op),
        deviceStepUp: (c) => api.deviceStepUp(c),
      })
      const collected = await pollForApproval(started, (code) => api.deviceToken<DeviceProof>(code), {
        sleep: async () => undefined,
      })
      return collected.proof
    })

    const out = await call<{ id: string }>('POST', '/api/authority/grants', { verdict: 'deny' })
    expect(out.id).toBe('g1')
    expect(seen.map((s) => s.path)).toEqual([
      '/api/authority/grants',
      '/api/auth/device/stepup',
      '/api/auth/device/token',
      '/api/auth/device/token',
      '/api/authority/grants',
    ])
    expect(JSON.parse(seen[1].body!)).toEqual({ challenge_id: 'c1' })
    // The secret goes in the poll's body, never in its address.
    expect(JSON.parse(seen[2].body!)).toEqual({ device_code: 'the-secret' })
    expect(seen.every((s) => !s.path.includes('the-secret'))).toBe(true)
    expect(seen[4].headers[STEP_UP_HEADER]).toBe('the-proof')
    expect(seen[4].body).toBe(seen[0].body)
  })
})

describe('the approving browser', () => {
  test('says what it is approving', () => {
    expect(approvalHeadline({ kind: 'sign-in', summary: null, action: null })).toBe('Sign in another window?')
    expect(approvalHeadline({ kind: 'step-up', summary: 'Remove a passkey', action: 'DELETE /api/passkeys/k' })).toBe(
      'Confirm for another window: Remove a passkey',
    )
    expect(approvalHeadline({ kind: 'step-up', summary: null, action: 'DELETE /api/passkeys/k' })).toBe(
      'Confirm for another window: DELETE /api/passkeys/k',
    )
  })

  test('says why it cannot approve, when it cannot', () => {
    const v = { status: 'pending', local: false, passkeys: 1, relying_party: true }
    expect(cannotApprove(v, true)).toBeNull()
    expect(cannotApprove({ ...v, local: true, passkeys: 0 }, false)).toBeNull()
    expect(cannotApprove({ ...v, passkeys: 0 }, true)).toContain('No passkey is enrolled')
    expect(cannotApprove({ ...v, relying_party: false }, true)).toContain('public origin')
    expect(cannotApprove(v, false)).toContain('cannot use passkeys')
    expect(cannotApprove({ ...v, status: 'denied' }, false)).toBeNull()
  })

  test('words a request that is no longer waiting', () => {
    expect(settled('pending')).toBeNull()
    expect(settled('approved')).toContain('Approved')
    expect(settled('consumed')).toContain('Approved')
    expect(settled('denied')).toContain('Denied')
    expect(settled('expired')).toContain('expired')
    expect(settled('revoked')).toContain('cancelled')
  })
})
