import { describe, expect, test } from 'bun:test'
import { openExternal, validateExternalUrl, type ReservedWindow } from './external'

describe('external provider URLs', () => {
  test('accepts only HTTPS URLs without userinfo', () => {
    expect(validateExternalUrl('https://login.example/path?state=1')).toBe(
      'https://login.example/path?state=1',
    )
    for (const value of [
      'http://login.example/path',
      'javascript:alert(1)',
      'https://user:secret@login.example/path',
      'not a url',
    ]) {
      expect(() => validateExternalUrl(value)).toThrow()
    }
  })

  test('closes a reserved window when validation fails', async () => {
    let closed = false
    const reserved: ReservedWindow = {
      opener: {},
      location: { href: 'about:blank' },
      close: () => {
        closed = true
      },
    }

    await expect(openExternal('http://login.example', reserved)).rejects.toThrow('unsafe')
    expect(closed).toBe(true)
    expect(reserved.location.href).toBe('about:blank')
  })
})

test('navigates a reserved window when its opener is read-only', async () => {
  let closed = false
  const reserved = {
    get opener(): unknown {
      return null
    },
    set opener(_value: unknown) {
      throw new Error('read-only')
    },
    location: { href: 'about:blank' },
    close: () => {
      closed = true
    },
  } satisfies ReservedWindow

  await openExternal('https://login.example/path', reserved)

  expect(reserved.location.href).toBe('https://login.example/path')
  expect(closed).toBe(false)
})
