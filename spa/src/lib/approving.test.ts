import { plugin } from 'bun'
import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'

import { compileModule } from 'svelte/compiler'

import { stashApproval, stashedApproval } from './device'

// A rune module is compiled the way the build compiles it: TypeScript out
// first, then Svelte's runes.
plugin({
  name: 'svelte-module',
  setup(b) {
    b.onLoad({ filter: /\.svelte\.ts$/ }, ({ path }) => {
      const js = new Bun.Transpiler({ loader: 'ts' }).transformSync(readFileSync(path, 'utf8'))
      return { contents: compileModule(js, { filename: path, generate: 'client' }).js.code, loader: 'js' }
    })
  },
})

const { approving, closeApproval, openApproval } = await import('./approving.svelte')

const id = 'a'.repeat(22)

describe('approving', () => {
  test('opening an approval shows it and holds it through a login', () => {
    stashApproval(null)
    openApproval(id)
    expect(approving.id).toBe(id)
    expect(stashedApproval()).toBe(id)
  })

  test('closing puts the interface back and forgets it', () => {
    openApproval(id)
    closeApproval()
    expect(approving.id).toBeNull()
    expect(stashedApproval()).toBeNull()
  })

  test('opening another replaces the one shown', () => {
    openApproval(id)
    openApproval('b'.repeat(22))
    expect(approving.id).toBe('b'.repeat(22))
    closeApproval()
  })

  test('opening one never touches the address', () => {
    // The router reads the fragment for the Settings section, so a scan that
    // set it would move Settings too.
    const source = readFileSync(new URL('./approving.svelte.ts', import.meta.url), 'utf8').replace(/\/\/.*$|\/\*[\s\S]*?\*\//gm, '')
    expect(source).not.toMatch(/location|history/)
  })
})
