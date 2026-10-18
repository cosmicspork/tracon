import { expect, test } from 'bun:test'
import { credentialReach } from './credentials'

test('a group prefix is shown with the configured repositories it covers', () => {
  expect(
    credentialReach({
      repos: ['gitlab.com/uitao/**'],
      covers: ['gitlab.com/uitao/portal', 'gitlab.com/uitao/api'],
    }),
  ).toBe('lent to gitlab.com/uitao/** · covers gitlab.com/uitao/portal, gitlab.com/uitao/api')
})

test('a prefix that reaches no configured repository says so', () => {
  expect(credentialReach({ repos: ['gitlab.com/uitao/**'], covers: [] })).toBe(
    'lent to gitlab.com/uitao/** · covers no configured repository',
  )
})

test('a credential lent to no repository shows no reach', () => {
  expect(credentialReach({ repos: [] })).toBe(null)
  expect(credentialReach({})).toBe(null)
})
