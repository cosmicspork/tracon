import { describe, expect, test } from 'bun:test'
import { render, renderMessage, renderWithoutFetching } from './markdown'

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

  test("a session's message renders without fetching, and its links open beside the log", () => {
    const html = renderMessage(
      '## Done\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```sh\njust check\n```\n\n![shot](https://example.test/x.png) [run](https://example.com/run) [bad](javascript:alert(1))',
    )
    expect(html).toContain('<h2>Done</h2>')
    expect(html).toContain('<li>one</li>')
    expect(html).toContain('<table>')
    expect(html).toContain('<code class="language-sh">just check')
    expect(html).not.toContain('<img')
    expect(html).not.toContain('example.test')
    expect(html).toContain('<a href="https://example.com/run" target="_blank" rel="noopener noreferrer">run</a>')
    expect(html).not.toContain('javascript:')
    expect(renderMessage('<script>alert(1)</script>')).not.toContain('<script>')
  })

  test('a single newline in a message stays a line break', () => {
    expect(renderMessage('first\nsecond')).toBe('<p>first<br>second</p>\n')
  })
})
