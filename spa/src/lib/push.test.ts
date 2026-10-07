import { expect, test } from 'bun:test'
import { ApiError } from './api'
import { enroll, EnrollError, keyBytes, needsInstall, supported, type EnrollEnv, type EnrollStage } from './push'

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
