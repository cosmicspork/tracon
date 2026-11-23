import { describe, expect, test } from 'bun:test'
import { codeSpans, render, renderForge, renderMessage, renderWithoutFetching } from './markdown'

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

describe('codeSpans', () => {
  test('sets code spans as code and escapes the rest', () => {
    expect(codeSpans('add `checks` to its [[repo]] <entry>')).toBe('add <code>checks</code> to its [[repo]] &lt;entry&gt;')
  })
})

describe('renderForge', () => {
  test('the generated-details fold renders as the forge folds it', () => {
    const html = renderForge(
      'Fixes the flaky sync.\n\n<details>\n<summary>Generated details</summary>\n\n## What\n\nLong text.\n\n</details>',
    )
    expect(html).toContain('<details>')
    expect(html).toContain('<summary>Generated details</summary>')
    expect(html).toContain('<h2>What</h2>')
    expect(html).toContain('</details>')
  })

  test('any other HTML, and attributes on the fold, stay text', () => {
    const html = renderForge('<details open onclick="x()">\n<script>alert(1)</script>\n<img src=x onerror=y>')
    expect(html).not.toContain('<details')
    expect(html).not.toContain('<script')
    expect(html).not.toContain('<img')
    expect(html).toContain('&lt;script&gt;')
  })

  test('nothing is fetched, unsafe links are dropped and a newline is a break', () => {
    expect(renderForge('![chart](https://example.com/a.png)')).toContain('<span class="no-fetch">[chart]</span>')
    expect(renderForge('[x](javascript:alert(1))')).not.toContain('href')
    expect(renderForge('[x](https://example.com)')).toContain('target="_blank"')
    expect(renderForge('one\ntwo')).toContain('<br>')
  })
})
