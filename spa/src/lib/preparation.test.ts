import { expect, test } from 'bun:test'
import { incompatibilityCount, preparationLine } from './preparation'
import type { PreparationPreview } from './types'

const preview = (over: Partial<PreparationPreview>): PreparationPreview => ({
  repo: '/r',
  image: 'img',
  image_source: 'harness image',
  devcontainer_image: null,
  lockfiles: [],
  install: null,
  prepare: [],
  egress: [],
  incompatible: [],
  ready: true,
  ...over,
})

const item = (blocking: boolean) => ({ source: 'f', item: 'i', reason: 'r', instead: null, blocking })

test('the line says what runs, in order, and where', () => {
  expect(preparationLine(preview({}))).toBe('no lockfile to install · in harness image')
  expect(preparationLine(preview({ install: 'npm ci --ignore-scripts', prepare: ['make deps'] }))).toBe(
    'npm ci --ignore-scripts, then make deps · in harness image',
  )
})

test('what would stop preparation is told apart from what is only passed over', () => {
  expect(incompatibilityCount(preview({}))).toBeNull()
  expect(incompatibilityCount(preview({ incompatible: [item(true), item(false), item(false)] }))).toBe(
    '1 would stop preparation · 2 not honoured',
  )
})
