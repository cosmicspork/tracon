import { describe, expect, test } from 'bun:test'
import { render, renderWithoutFetching } from './markdown'

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

  test('an account of shown work names its images instead of fetching them', () => {
    const html = renderWithoutFetching('![before](https://example.test/a.png?d=secret) and [link](https://example.com)')
    expect(html).not.toContain('<img')
    expect(html).not.toContain('example.test')
    expect(html).toContain('[before]')
    expect(html).toContain('<a href="https://example.com">link</a>')
    expect(renderWithoutFetching('<img src=x onerror="alert(1)">')).not.toContain('<img')
  })
})
