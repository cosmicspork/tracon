import { describe, expect, test } from 'bun:test'
import { render } from './markdown'

describe('markdown rendering', () => {
  test('escapes embedded HTML instead of trusting document authors', () => {
    const html = render('<img src=x onerror="alert(1)"><script>alert(2)</script>')
    expect(html).not.toContain('<script>')
    expect(html).not.toContain('<img')
    expect(html).not.toContain('onerror="')
    expect(html).toContain('&lt;script&gt;')
  })

  test('drops executable markdown URLs but keeps ordinary links', () => {
    expect(render('[run](javascript:alert(1))')).not.toContain('href=')
    expect(render('![run](data:text/html,x)')).not.toContain('<img')
    expect(render('[docs](https://example.com/docs)')).toContain(
      '<a href="https://example.com/docs">docs</a>',
    )
  })
})
