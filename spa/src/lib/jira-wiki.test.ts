import { describe, expect, test } from 'bun:test'
import { renderJiraWiki } from './jira-wiki'

describe('jira wiki rendering', () => {
  test('headings and inline emphasis', () => {
    expect(renderJiraWiki('h2. Scope *now*')).toBe('<h2>Scope <strong>now</strong></h2>')
    expect(renderJiraWiki('a *bold* and _italic_ word')).toBe(
      '<p>a <strong>bold</strong> and <em>italic</em> word</p>',
    )
    expect(renderJiraWiki('-gone- and +added+')).toBe('<p><del>gone</del> and <ins>added</ins></p>')
  })

  test('markers inside words are left alone', () => {
    expect(renderJiraWiki('snake_case_name and a-b-c and 2*3*4')).toBe(
      '<p>snake_case_name and a-b-c and 2*3*4</p>',
    )
  })

  test('inline code is literal and escaped', () => {
    expect(renderJiraWiki('run {{*not bold* <b>}} now')).toBe(
      '<p>run <code>*not bold* &lt;b&gt;</code> now</p>',
    )
  })

  test('code and noformat blocks keep their text verbatim', () => {
    const src = 'before\n{code:php}\n$a = "<x>";\n*not bold*\n{code}\nafter'
    expect(renderJiraWiki(src)).toBe(
      '<p>before</p>\n<pre><code>$a = &quot;&lt;x&gt;&quot;;\n*not bold*</code></pre>\n<p>after</p>',
    )
    expect(renderJiraWiki('{noformat}h1. raw{noformat}')).toBe('<pre><code>h1. raw</code></pre>')
    expect(renderJiraWiki('{code}\nunterminated')).toBe('<pre><code>unterminated</code></pre>')
  })

  test('nested and mixed lists', () => {
    expect(renderJiraWiki('* one\n** one.a\n* two\n# first')).toBe(
      '<ul><li>one<ul><li>one.a</li></ul></li><li>two</li></ul><ol><li>first</li></ol>',
    )
    expect(renderJiraWiki('- dash\n- item')).toBe('<ul><li>dash</li><li>item</li></ul>')
    expect(renderJiraWiki('*bold* is not a list')).toBe('<p><strong>bold</strong> is not a list</p>')
  })

  test('tables with header cells and links that contain bars', () => {
    expect(renderJiraWiki('||Name||Link||\n|a|[docs|https://example.com/x]|')).toBe(
      '<table><tbody><tr><th>Name</th><th>Link</th></tr>' +
        '<tr><td>a</td><td><a href="https://example.com/x">docs</a></td></tr></tbody></table>',
    )
  })

  test('links are allow-listed the same way as markdown', () => {
    expect(renderJiraWiki('[docs|https://example.com/a?b=1&c=2]')).toBe(
      '<p><a href="https://example.com/a?b=1&amp;c=2">docs</a></p>',
    )
    expect(renderJiraWiki('[https://example.com]')).toBe(
      '<p><a href="https://example.com">https://example.com</a></p>',
    )
    expect(renderJiraWiki('[run|javascript:alert(1)]')).toBe('<p>run</p>')
    expect(renderJiraWiki('[run|java\nscript:alert(1)]')).not.toContain('href')
    expect(renderJiraWiki('[x|data:text/html,<script>]')).not.toContain('href')
    expect(renderJiraWiki('[x|//evil.example]')).toBe('<p>x</p>')
    expect(renderJiraWiki('[home|/work]')).toBe('<p><a href="/work">home</a></p>')
    expect(renderJiraWiki('see [~someone]')).toBe('<p>see <span class="mention">@someone</span></p>')
    expect(renderJiraWiki('a [plain] bracket')).toBe('<p>a [plain] bracket</p>')
  })

  test('bare URLs link without swallowing trailing punctuation', () => {
    expect(renderJiraWiki('see https://example.com/x.')).toBe(
      '<p>see <a href="https://example.com/x">https://example.com/x</a>.</p>',
    )
  })

  test('embedded HTML is escaped, never trusted', () => {
    const html = renderJiraWiki('<img src=x onerror="alert(1)"><script>alert(2)</script>')
    expect(html).not.toContain('<script>')
    expect(html).not.toContain('<img')
    expect(html).toContain('&lt;script&gt;')
    expect(renderJiraWiki('[<b>x</b>|https://example.com/"onmouseover="]')).toBe(
      '<p><a href="https://example.com/&quot;onmouseover=&quot;">&lt;b&gt;x&lt;/b&gt;</a></p>',
    )
    expect(renderJiraWiki('\u00000\u0000 *x*')).toBe('<p>0 <strong>x</strong></p>')
  })

  test('paragraphs keep single line breaks and split on blank lines', () => {
    expect(renderJiraWiki('one\ntwo\n\nthree\r\n----\nbq. quoted')).toBe(
      '<p>one<br>two</p>\n<p>three</p>\n<hr>\n<blockquote>quoted</blockquote>',
    )
  })
})
