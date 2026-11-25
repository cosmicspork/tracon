import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { OFFERS_LINK, buildStepsDiff, decidedLine, decidedOnto, forgeIdentity, inboxOrder, offerResultLine, offerTargets, reviewIsStale, shown, waitingOffers } from './recipes'
import type { ChannelInfo, NodeInfo, RecipeOffer, RecipeOfferView } from './types'

const channel = (name: string, nodes: string[], archived: number | null = null) =>
  ({ name, nodes, archived, bindings: {}, ceiling: {} }) as unknown as ChannelInfo
const node = (id: string, name: string) => ({ id, name }) as NodeInfo

test('a recipe is offered only on a channel shared with another member', () => {
  const targets = offerTargets(
    [
      channel('personal', ['me', 'laptop']),
      channel('solo', ['me']),
      channel('theirs', ['laptop', 'homelab']),
      channel('old', ['me', 'laptop'], 1),
      channel('@mesh', ['me', 'laptop']),
    ],
    [node('me', 'here'), node('laptop', 'laptop')],
    'me',
  )
  expect(targets).toEqual([{ channel: 'personal', peers: [{ id: 'laptop', name: 'laptop' }] }])
})

test('a path names its repository only when it is a forge identity', () => {
  expect(forgeIdentity('github.com/acme/app')).toBe('github.com/acme/app')
  expect(forgeIdentity('gitlab.example.org/group/sub/app')).toBe('gitlab.example.org/group/sub/app')
  for (const path of ['/var/home/op/app', 'acme/app', 'GitHub.com/acme/app', 'github.com/acme/.x']) {
    expect(forgeIdentity(path)).toBeNull()
  }
})

test('values read as the comparison shows them', () => {
  expect(shown(undefined)).toBe('not set')
  expect(shown([])).toBe('none')
  expect(shown(['a', 'b'])).toBe('a\nb')
  expect(shown(true)).toBe('yes')
  expect(shown(600)).toBe('600')
})

test('undecided offers come first, newest first', () => {
  const offer = (id: string, state: RecipeOffer['state'], received_ms: number) =>
    ({ id, state, received_ms }) as RecipeOffer
  const ordered = inboxOrder([offer('a', 'declined', 30), offer('b', 'pending', 10), offer('c', 'pending', 20)])
  expect(ordered.map((o) => o.id)).toEqual(['c', 'b', 'a'])
})

test('each node an offer went to is reported', () => {
  expect(offerResultLine({ name: 'homelab', node: 'n', state: 'offered' })).toContain('its operator decides')
  expect(offerResultLine({ name: '', node: 'abcdef0123456789', state: 'queued' })).toMatch(/^node-[a-z]+-[a-z]+-\d\d: /)
  expect(offerResultLine({ name: 'old', node: 'n', state: 'refused', detail: 'older build' })).toContain('older build')
})

test('an accept waits for a review of exactly what it would write', () => {
  const view = { target: { path: 'github.com/acme/app', exists: true }, replace_image: false, keep_credentials: false } as RecipeOfferView
  expect(reviewIsStale(null, '', false, false)).toBe(true)
  expect(reviewIsStale(view, 'github.com/acme/app ', false, false)).toBe(false)
  expect(reviewIsStale(view, '/srv/app', false, false)).toBe(true)
  expect(reviewIsStale(view, 'github.com/acme/app', true, false)).toBe(true)
  expect(reviewIsStale(view, 'github.com/acme/app', false, true)).toBe(true)
  // Build steps apply by default; opting out is a different review.
  expect(reviewIsStale(view, 'github.com/acme/app', false, false, true)).toBe(false)
  expect(reviewIsStale(view, 'github.com/acme/app', false, false, false)).toBe(true)
  expect(reviewIsStale({ ...view, apply_build: false }, 'github.com/acme/app', false, false, false)).toBe(false)
})

test("the push for a staged offer opens the same place as Home's card", () => {
  const rust = readFileSync(new URL('../../../node/src/http/recipes.rs', import.meta.url), 'utf8')
  expect(rust).toContain(`"${OFFERS_LINK}".into()`)
})

describe('decided offers', () => {
  const at = (id: string, state: RecipeOffer['state'], decided: number | null, path: string | null): RecipeOffer => ({
    id,
    sender_node: 'n1',
    sender_name: 'bazzite',
    channel: 'personal',
    repo: 'github.com/o/a',
    recipe: {} as RecipeOffer['recipe'],
    note: null,
    received_ms: decided ?? 0,
    expires_ms: 0,
    state,
    decided_ms: decided,
    decided_path: path,
  })
  const offers = [
    at('p', 'pending', null, null),
    at('old', 'superseded', 10, 'github.com/o/a'),
    at('new', 'accepted', 20, 'github.com/o/a'),
    at('other', 'accepted', 30, 'github.com/o/b'),
    at('no', 'declined', 40, null),
  ]

  test('the inbox lists only what waits on a decision', () => {
    expect(waitingOffers(offers).map((o) => o.id)).toEqual(['p'])
    expect(waitingOffers([at('a', 'accepting', 1, null)]).map((o) => o.id)).toEqual(['a'])
  })

  test('an entry shows the latest offer accepted onto it, the rest as history', () => {
    const { latest, earlier } = decidedOnto(offers, 'github.com/o/a')
    expect(latest?.id).toBe('new')
    expect(earlier.map((o) => o.id)).toEqual(['old'])
    expect(decidedOnto(offers, 'github.com/o/c')).toEqual({ latest: null, earlier: [] })
    expect(decidedOnto(offers, '')).toEqual({ latest: null, earlier: [] })
    expect(decidedLine(latest!)).toBe('recipe from bazzite on personal · accepted')
    expect(decidedLine(earlier[0])).toBe('recipe from bazzite on personal · accepted, since replaced')
  })
})

test('offered build steps are compared with the entry own, line by line', () => {
  expect(buildStepsDiff(['apt-get update', 'apk add jq'], ['apt-get update', 'apt-get install -y jq'])).toEqual([
    { step: 'apk add jq', mark: 'removed' },
    { step: 'apt-get update', mark: 'same' },
    { step: 'apt-get install -y jq', mark: 'added' },
  ])
  expect(buildStepsDiff([], ['make'])).toEqual([{ step: 'make', mark: 'added' }])
})
