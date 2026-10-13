import { expect, test } from 'bun:test'
import { ApiError } from './api'
import { enroll, EnrollError, keyBytes, madeWith, needsInstall, supported, type EnrollEnv, type EnrollStage } from './push'

test('the node key decodes to an uncompressed P-256 point', () => {
  const key = 'BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4'
  const bytes = keyBytes(key)
  expect(bytes.length).toBe(65)
  expect(bytes[0]).toBe(0x04)
})

test('padding is tolerated either way', () => {
  expect(keyBytes('AQ')).toEqual(new Uint8Array([1]))
  expect(keyBytes('AQ==')).toEqual(new Uint8Array([1]))
})

test('outside a browser nothing is supported and nothing needs installing', () => {
  expect(supported()).toBe(false)
  expect(needsInstall()).toBe(false)
})

/** A browser where every step works, and a record of what ran. */
function env(over: Partial<EnrollEnv> = {}, unsubscribed: string[] = []): EnrollEnv {
  const sub = {
    endpoint: 'https://push.test/1',
    toJSON: () => ({ endpoint: 'https://push.test/1', keys: { p256dh: 'p', auth: 'a' } }),
    unsubscribe: async () => {
      unsubscribed.push('https://push.test/1')
      return true
    },
  } as unknown as PushSubscription
  return {
    supported: () => true,
    permission: async () => 'granted',
    ready: async () => ({
      pushManager: {
        getSubscription: async () => null,
        subscribe: async () => sub,
      } as unknown as PushManager,
    }),
    key: async () => 'AQ',
    register: async () => ({ id: 'd1' }),
    forget: async () => undefined,
    ...over,
  }
}

async function stageOf(e: EnrollEnv): Promise<EnrollStage | 'enrolled'> {
  try {
    await enroll(e)
    return 'enrolled'
  } catch (cause) {
    expect(cause).toBeInstanceOf(EnrollError)
    return (cause as EnrollError).stage
  }
}

test('each way enrolling fails is named by its stage', async () => {
  expect(await stageOf(env())).toBe('enrolled')
  expect(await stageOf(env({ supported: () => false }))).toBe('unavailable')
  expect(await stageOf(env({ permission: async () => 'denied' }))).toBe('denied')
  expect(await stageOf(env({ permission: async () => 'default' }))).toBe('denied')
  expect(await stageOf(env({ ready: () => Promise.reject(new Error('not ready after 10 s')) }))).toBe('service-worker')
  const refused = env({
    ready: async () => ({
      pushManager: {
        getSubscription: async () => null,
        subscribe: () => Promise.reject(new DOMException('push service error', 'AbortError')),
      } as unknown as PushManager,
    }),
  })
  expect(await stageOf(refused)).toBe('push-service')
  expect(await stageOf(env({ key: () => Promise.reject(new TypeError('Failed to fetch')) }))).toBe('node')
})

test('a device the node did not record is never left looking registered', async () => {
  const unsubscribed: string[] = []
  const failing = env({ register: () => Promise.reject(new ApiError(500, 'database is locked')) }, unsubscribed)
  let error: EnrollError | null = null
  try {
    await enroll(failing)
  } catch (cause) {
    error = cause as EnrollError
  }
  expect(error?.stage).toBe('node')
  expect(error?.message).toContain('its storage failed: database is locked')
  expect(error?.message).toContain('not registered')
  expect(unsubscribed).toEqual(['https://push.test/1'])

  const enrolled: string[] = []
  await enroll(env({}, enrolled))
  expect(enrolled).toEqual([])
})

/** A subscription a browser already holds, made with `key`. */
function held(endpoint: string, key: string | null, log: string[]): PushSubscription {
  return {
    endpoint,
    options: { userVisibleOnly: true, applicationServerKey: key === null ? null : keyBytes(key).buffer },
    toJSON: () => ({ endpoint, keys: { p256dh: 'p', auth: 'a' } }),
    unsubscribe: async () => {
      log.push(`unsubscribe ${endpoint}`)
      return true
    },
  } as unknown as PushSubscription
}

test('a subscription says whether it was made with the node key', () => {
  expect(madeWith(held('e', 'AQID', []), 'AQID')).toBe(true)
  expect(madeWith(held('e', 'AQID', []), 'AQIE')).toBe(false)
  expect(madeWith(held('e', null, []), 'AQID')).toBeNull()
})

/** Enroll against a browser already holding `existing`, with the node on `key`. */
async function enrollHolding(existing: PushSubscription, key: string, log: string[]) {
  const fresh = held('https://push.test/new', key, log)
  await enroll(
    env({
      key: async () => key,
      ready: async () => ({
        pushManager: {
          getSubscription: async () => existing,
          subscribe: async (options?: PushSubscriptionOptionsInit) => {
            log.push(`subscribe ${new Uint8Array(options?.applicationServerKey as ArrayBuffer).join(',')}`)
            return fresh
          },
        } as unknown as PushManager,
      }),
      forget: async (endpoint) => log.push(`forget ${endpoint}`),
      register: async (sub) => log.push(`register ${sub.endpoint}`),
    }),
  )
}

test('a subscription made with an old node key is replaced, here and on the node', async () => {
  const log: string[] = []
  await enrollHolding(held('https://push.test/old', 'AQID', log), 'BAUG', log)
  expect(log).toEqual([
    'forget https://push.test/old',
    'unsubscribe https://push.test/old',
    'subscribe 4,5,6',
    'register https://push.test/new',
  ])
})

test('a subscription made with the current key is kept', async () => {
  const log: string[] = []
  await enrollHolding(held('https://push.test/same', 'BAUG', log), 'BAUG', log)
  expect(log).toEqual(['register https://push.test/same'])
  // A browser that does not say which key it used is not churned either.
  const unknown: string[] = []
  await enrollHolding(held('https://push.test/quiet', null, unknown), 'BAUG', unknown)
  expect(unknown).toEqual(['register https://push.test/quiet'])
})
