// The rules the shell route depends on, away from the component: which paths
// are the shell, which boot URLs may be framed, and — the one that is a
// security property rather than a convenience — that the capability in a boot
// URL never comes back out of anything that writes it down.

import { expect, test, describe } from 'bun:test'
import {
  BootUrlRefused,
  DEFAULT_COOKIE_TTL_MS,
  frameSrc,
  inScope,
  opencodeShellPath,
  redact,
  shellSessionId,
  staleAfter,
} from './opencode'

const SELF = 'http://127.0.0.1:7420'
const UI = 'http://127.0.0.2:7423'
const minted = (over: Partial<{ url: string; origin: string }> = {}) => ({
  url: `${UI}/#/session/ses_abc?boot=TOKEN-TOKEN-TOKEN`,
  origin: UI,
  expires_ms: 1_000,
  ...over,
})

describe('the shell route', () => {
  test('is a path on this origin, so an installed app keeps the session', () => {
    expect(opencodeShellPath('ses_abc')).toBe('/sessions/ses_abc/opencode')
    // The manifest claims `/`, and this is under it. Said as an assertion
    // because the deep link and the installed app both depend on it.
    expect(inScope(opencodeShellPath('ses_abc'))).toBe(true)
    expect(inScope('https://elsewhere.example/sessions/x/opencode')).toBe(false)
  })

  test('round-trips a session id, including one that needs escaping', () => {
    for (const id of ['ses_abc', 'a b', 'a/b', 'ünïcode']) {
      expect(shellSessionId(opencodeShellPath(id))).toBe(id)
    }
  })

  test('is not the session screen, and not a path that merely starts like it', () => {
    expect(shellSessionId('/sessions/ses_abc')).toBeNull()
    expect(shellSessionId('/sessions/ses_abc/opencode/extra')).toBeNull()
    expect(shellSessionId('/sessions')).toBeNull()
    expect(shellSessionId('/opencode')).toBeNull()
    // A trailing slash is the same route.
    expect(shellSessionId('/sessions/ses_abc/opencode/')).toBe('ses_abc')
  })
})

describe('a boot URL is checked before it is framed', () => {
  test('the node s own is accepted, whole', () => {
    expect(frameSrc(minted(), SELF)).toBe(`${UI}/#/session/ses_abc?boot=TOKEN-TOKEN-TOKEN`)
  })

  test('a scheme that is not http(s) is refused', () => {
    expect(() => frameSrc(minted({ url: 'file:///etc/passwd#?boot=x', origin: 'null' }), SELF)).toThrow(
      BootUrlRefused,
    )
  })

  test('a URL that drifted from the origin the node named is refused', () => {
    expect(() =>
      frameSrc(minted({ url: 'http://evil.example/#?boot=x' }), SELF),
    ).toThrow(/not the http:\/\/127\.0\.0\.2:7423 it named/)
  })

  test('a boot URL sharing this origin is refused: the isolation is the origin', () => {
    expect(() =>
      frameSrc(minted({ url: `${SELF}/#?boot=x`, origin: SELF }), SELF),
    ).toThrow(/its own origin/)
  })

  test('a capability in the query is refused, because a query is logged', () => {
    expect(() =>
      frameSrc(minted({ url: `${UI}/?boot=TOKEN#/session/ses_abc` }), SELF),
    ).toThrow(/where it would be logged/)
  })

  test('a URL with no capability at all is refused rather than framed blank', () => {
    expect(() => frameSrc(minted({ url: `${UI}/#/session/ses_abc` }), SELF)).toThrow(
      /no capability/,
    )
    expect(() => frameSrc(minted({ url: 'not a url', origin: UI }), SELF)).toThrow(BootUrlRefused)
  })
})

describe('the capability never leaves the src', () => {
  test('what is written down has no fragment and no query', () => {
    const url = `${UI}/#/session/ses_abc?boot=SECRET-TOKEN`
    const shown = redact(url)
    expect(shown).toBe(`${UI}/`)
    expect(shown).not.toContain('SECRET-TOKEN')
    expect(shown).not.toContain('boot')
    expect(shown).not.toContain('#')
  })

  test('a query-carried one is stripped too, and so is a URL that will not parse', () => {
    expect(redact(`${UI}/x?boot=SECRET#frag`)).toBe(`${UI}/x`)
    expect(redact('nonsense')).toBe('<not a URL>')
  })
})

describe('when a backgrounded frame is certainly finished', () => {
  test('the node s own cookie lifetime decides it', () => {
    expect(staleAfter(1_000, 60_000)).toBe(61_000)
  })

  test('a node that did not say is assumed to agree with the node that did', () => {
    expect(staleAfter(0)).toBe(DEFAULT_COOKIE_TTL_MS)
    expect(staleAfter(0, 0)).toBe(DEFAULT_COOKIE_TTL_MS)
  })
})
