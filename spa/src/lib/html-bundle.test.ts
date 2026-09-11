import { expect, test } from 'bun:test'
import {
  MAX_BUNDLE_BYTES,
  MAX_FILE_BYTES,
  MAX_FILES,
  selectHtmlFile,
  selectHtmlFolder,
  suggestSlug,
} from './html-bundle'

function fakeFile(name: string, size = 1, webkitRelativePath = ''): File {
  return { name, size, webkitRelativePath } as File
}

test('single-file selection accepts exactly one HTML file', () => {
  const selected = selectHtmlFile([fakeFile('Report.HTML', 12)])
  expect(selected.sourceName).toBe('Report.HTML')
  expect(selected.entryPath).toBe('Report.HTML')
  expect(selected.totalBytes).toBe(12)
  expect(selected.suggestedSlug).toBe('ref-report')
  expect(() => selectHtmlFile([fakeFile('report.txt')])).toThrow('exactly one .html')
  expect(() => selectHtmlFile([fakeFile('a.html'), fakeFile('b.html')])).toThrow('exactly one .html')
})

test('folder selection prefers root index.html', () => {
  const selected = selectHtmlFolder([
    fakeFile('index.html', 4, 'Demo/index.html'),
    fakeFile('nested.html', 5, 'Demo/pages/nested.html'),
    fakeFile('style.css', 6, 'Demo/assets/style.css'),
  ])
  expect(selected.sourceName).toBe('Demo')
  expect(selected.entryPath).toBe('index.html')
  expect(selected.files.map((file) => file.path)).toEqual([
    'index.html',
    'pages/nested.html',
    'assets/style.css',
  ])
})

test('folder selection accepts one nested HTML entry', () => {
  const selected = selectHtmlFolder([
    fakeFile('page.html', 4, 'Demo/pages/page.html'),
    fakeFile('style.css', 6, 'Demo/style.css'),
  ])
  expect(selected.entryPath).toBe('pages/page.html')
})

test('folder selection rejects missing and ambiguous entries', () => {
  expect(() => selectHtmlFolder([fakeFile('style.css', 1, 'Demo/style.css')])).toThrow(
    'does not contain an HTML',
  )
  expect(() =>
    selectHtmlFolder([
      fakeFile('a.html', 1, 'Demo/a.html'),
      fakeFile('b.html', 1, 'Demo/b.html'),
    ]),
  ).toThrow('exactly one HTML')
})

test('folder paths strip one root and reject unsafe normalization', () => {
  expect(() => selectHtmlFolder([fakeFile('index.html', 1, 'A/../index.html')])).toThrow(
    'path is invalid',
  )
  expect(() =>
    selectHtmlFolder([
      fakeFile('index.html', 1, 'A/index.html'),
      fakeFile('other.css', 1, 'B/other.css'),
    ]),
  ).toThrow('one common root')
})

test('slug suggestions preserve document kinds and otherwise use ref', () => {
  expect(suggestSlug('plan-NUDEV 22')).toBe('plan-nudev-22')
  expect(suggestSlug('Quarterly Report')).toBe('ref-quarterly-report')
  expect(suggestSlug('***')).toBe('ref-document')
})

test('selection refuses file count and byte limits', () => {
  expect(() =>
    selectHtmlFolder(
      Array.from({ length: MAX_FILES + 1 }, (_, index) =>
        fakeFile(`${index}.txt`, 1, `Demo/${index}.txt`),
      ),
    ),
  ).toThrow(`at most ${MAX_FILES}`)
  expect(() => selectHtmlFile([fakeFile('large.html', MAX_FILE_BYTES + 1)])).toThrow('file limit')
  expect(() =>
    selectHtmlFolder([
      fakeFile('index.html', MAX_BUNDLE_BYTES / 2 + 1, 'Demo/index.html'),
      fakeFile('asset.bin', MAX_BUNDLE_BYTES / 2, 'Demo/asset.bin'),
    ]),
  ).toThrow('bundle limit')
})
