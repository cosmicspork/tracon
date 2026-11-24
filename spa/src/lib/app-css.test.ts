import { expect, test } from 'bun:test'
import { appCss } from './app-css'

test("the stylesheet tests render with has @tracon/ui's tokens and base inlined", () => {
  const css = appCss()
  expect(css).not.toContain('@import')
  expect(css).toContain('--wash-wait')
  expect(css).toContain('.btn.p')
  expect(css).toContain('textarea.grow')
})
