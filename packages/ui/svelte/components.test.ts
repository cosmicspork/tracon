import { describe, expect, test } from 'bun:test'
import { createRawSnippet } from 'svelte'
import { render } from 'svelte/server'
import { segmentedKey } from '../segmented'
import { Banner, Empty, LoadGate, Row, SectionHeader, Segmented, SideButton, Toggle, WordDiff } from './index.js'

const html = (component: unknown, props: Record<string, unknown> = {}) =>
  render(component as never, { props } as never)
    .body.replace(/<!--[^>]*-->/g, '')
    // Scoping classes are the compiler's, not the component's markup.
    .replace(/ class="svelte-[a-z0-9]+"/g, '')
    .replace(/ svelte-[a-z0-9]+/g, '')
    .trim()
const text = (s: string) => createRawSnippet(() => ({ render: () => `<span>${s}</span>` }))

describe('Row', () => {
  test('a link, a button, or plain and dimmed when off', () => {
    expect(html(Row, { href: '/x', children: text('t') })).toContain('<a class="tr-row')
    expect(html(Row, { onclick: () => {}, expanded: true, children: text('t') })).toMatch(/<button[^>]*aria-expanded="true"/)
    const off = html(Row, { href: '/x', off: true, children: text('t') })
    expect(off).not.toContain('<a')
    expect(off).toMatch(/class="tr-row[^"]* off/)
    expect(off).toContain('aria-disabled="true"')
  })

  test('renders its lead, content and side buttons in order, and only the row without them', () => {
    const full = html(Row, { href: '/x', lead: text('lead'), children: text('body'), actions: text('side') })
    expect(full.indexOf('lead')).toBeLessThan(full.indexOf('body'))
    expect(full.indexOf('body')).toBeLessThan(full.indexOf('side'))
    // Full width without actions: the line holds the row and nothing else.
    const bare = html(Row, { href: '/x', children: text('body') })
    expect(bare).toMatch(/^<div class="tr-line"><a [^>]*><span>body<\/span><\/a>\s*<\/div>$/)
  })

  test('takes a gap and a height floor', () => {
    expect(html(Row, { href: '/x', gap: 14, minHeight: 52 })).toContain('style="gap: 0 14px; min-height: 52px;"')
  })
})

test('SideButton is a link with href, else a button', () => {
  expect(html(SideButton, { href: '/y', label: 'Open on work', children: text('work') })).toMatch(/<a class="tr-side[^"]*" href="\/y" aria-label="Open on work"/)
  expect(html(SideButton, { onclick: () => {}, children: text('dismiss') })).toMatch(/<button class="tr-side[^"]*" type="button"/)
})

test('Banner takes a tone, a bold detail and an action', () => {
  const out = html(Banner, { tone: 'crit', detail: 'database is locked', role: 'alert', children: text('Could not load'), action: text('Retry') })
  expect(out).toMatch(/class="banner crit[^"]*" role="alert"/)
  expect(out).toContain('<b>· database is locked</b>')
  expect(out.indexOf('database')).toBeLessThan(out.indexOf('Retry'))
  expect(html(Banner, { children: text('plain') })).toMatch(/class="banner [^"]*"/)
})

test('Empty says it is loading as a status', () => {
  expect(html(Empty, { children: text('None yet.') })).toBe('<div class="empty"><span>None yet.</span></div>')
  expect(html(Empty, { loading: true, children: text('Loading…') })).toContain('role="status"')
  expect(html(Empty, { children: text('None.'), action: text('Start one') })).toContain('Start one')
})

test('SectionHeader puts its detail in bold and its aside to the right', () => {
  const out = html(SectionHeader, { detail: 3, children: text('Running'), aside: text('all sessions') })
  expect(out).toContain('<b>3</b>')
  expect(out).toMatch(/<span class="tr-aside[^"]*"><span>all sessions<\/span><\/span>/)
  expect(html(SectionHeader, { children: text('Running') })).not.toContain('<b>')
})

describe('Segmented', () => {
  const options = [
    { value: 'auto', label: 'Match system' },
    { value: 'light', label: 'Light' },
    { value: 'dark', label: 'Dark' },
  ]

  test('is a radiogroup with the chosen one checked and alone in the tab order', () => {
    const out = html(Segmented, { options, value: 'light', label: 'Colour scheme', onchange: () => {} })
    expect(out).toContain('role="radiogroup" aria-label="Colour scheme"')
    expect(out.match(/role="radio"/g)?.length).toBe(3)
    expect(out).toMatch(/aria-checked="true" tabindex="0" class="on">Light/)
    expect(out.match(/tabindex="0"/g)?.length).toBe(1)
  })

  test('arrow keys move the choice and wrap; Home and End go to the ends', () => {
    expect(segmentedKey('ArrowRight', 0, 3)).toBe(1)
    expect(segmentedKey('ArrowDown', 2, 3)).toBe(0)
    expect(segmentedKey('ArrowLeft', 0, 3)).toBe(2)
    expect(segmentedKey('ArrowUp', 1, 3)).toBe(0)
    expect(segmentedKey('Home', 2, 3)).toBe(0)
    expect(segmentedKey('End', 0, 3)).toBe(2)
    expect(segmentedKey('Enter', 0, 3)).toBeNull()
    expect(segmentedKey('ArrowRight', -1, 3)).toBe(1)
    expect(segmentedKey('ArrowRight', 0, 0)).toBeNull()
  })
})

test('Toggle is a labelled checkbox with a title and a hint', () => {
  const out = html(Toggle, { checked: true, title: 'Open the window at launch', hint: 'Off: menu bar only.', onchange: () => {} })
  expect(out).toMatch(/^<label class="toggle"><input type="checkbox" checked(="")?\/?>/)
  expect(out).toContain('<b>Open the window at launch</b><small>Off: menu bar only.</small>')
  expect(html(Toggle, { checked: false, title: 'Quit', onchange: () => {} })).not.toContain('<small>')
})

describe('LoadGate', () => {
  test('says it is loading, then shows the answer', () => {
    expect(html(LoadGate, { status: { loaded: false, error: null }, what: 'work', children: text('rows') })).toContain('Loading work…')
    expect(html(LoadGate, { status: { loaded: true, error: null }, what: 'work', children: text('rows') })).toContain('rows')
  })
  test('a failure says why; one after an answer keeps it under the error', () => {
    const failed = html(LoadGate, { status: { loaded: false, error: 'locked' }, what: 'work', onretry: () => {}, children: text('rows') })
    expect(failed).toContain('Could not load work <b>· locked</b>')
    expect(failed).toContain('Retry')
    expect(failed).not.toContain('rows')
    const stale = html(LoadGate, { status: { loaded: true, error: 'locked' }, what: 'work', children: text('rows') })
    expect(stale).toContain('Could not refresh work')
    expect(stale).toContain('rows')
  })
})

test('WordDiff marks what was taken out and put in', () => {
  const out = html(WordDiff, { before: 'the quick fox', after: 'the slow fox' })
  expect(out).toContain('<del>quick</del>')
  expect(out).toContain('<ins>slow</ins>')
  expect(out).toContain('aria-label="Word diff: 1 words out, 1 in"')
  expect(html(WordDiff, { before: 'same', after: 'same' })).toContain('no change ·')
})
