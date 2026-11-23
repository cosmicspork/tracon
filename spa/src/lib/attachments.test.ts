import { expect, test } from 'bun:test'
import {
  addAttachments,
  looksAccepted,
  MAX_ATTACHMENT_BYTES,
  MAX_ATTACHMENTS,
  pastedName,
  sizeText,
  stagedOf,
  toBase64,
} from './attachments'
import { eventLine } from './log'
import type { Event } from './types'

const file = (name: string, size = 10, type = '') => ({ name, size, type })

test('screenshots, PDFs and logs are attached; other files are refused with why', () => {
  const { files, refused } = addAttachments(
    [],
    [file('shot.png', 10, 'image/png'), file('run.log'), file('doc.pdf', 10, 'application/pdf'), file('tool.exe', 10, 'application/x-msdownload')],
  )
  expect(files.map((f) => f.name)).toEqual(['shot.png', 'run.log', 'doc.pdf'])
  expect(refused).toEqual(['tool.exe is not a type the node stages; it takes PNG, JPEG, GIF or WebP images, PDF, or text (a log, JSON, CSV, Markdown)'])
  // A file that says nothing of itself is left to the node, which reads its bytes.
  expect(looksAccepted(file('Makefile'))).toBe(true)
  expect(looksAccepted(file('archive.zip', 10, 'application/zip'))).toBe(false)
})

test('the node limits are held to before anything is sent, each refusal saying which', () => {
  const big = addAttachments([], [file('huge.log', MAX_ATTACHMENT_BYTES + 1)])
  expect(big.files).toEqual([])
  expect(big.refused[0]).toBe('huge.log is 10.0 MiB; one attachment may be at most 10.0 MiB')

  const many = addAttachments([], Array.from({ length: MAX_ATTACHMENTS + 1 }, (_, i) => file(`${i}.txt`)))
  expect(many.files.length).toBe(MAX_ATTACHMENTS)
  expect(many.refused).toEqual([`${MAX_ATTACHMENTS}.txt: a prompt carries at most ${MAX_ATTACHMENTS} files`])

  const nine = 9 * 1024 * 1024
  const total = addAttachments([file('a.log', nine), file('b.log', nine)], [file('c.log', nine)])
  expect(total.files.length).toBe(2)
  expect(total.refused).toEqual(['c.log: the attachments would add up to more than 20.0 MiB'])
})

test('sizes read as the node writes them', () => {
  expect(sizeText(16)).toBe('16 bytes')
  expect(sizeText(2048)).toBe('2.0 KiB')
  expect(sizeText(3 * 1024 * 1024)).toBe('3.0 MiB')
})

test('a pasted screenshot is named for when it was pasted', () => {
  expect(pastedName('image/png', new Date(2026, 10, 23, 9, 5, 7))).toBe('pasted-20261123-090507.png')
  expect(pastedName('image/jpeg', new Date(2026, 0, 2, 3, 4, 5))).toBe('pasted-20260102-030405.jpg')
})

test('bytes go as base64, a large file included', () => {
  expect(toBase64(new TextEncoder().encode('hello'))).toBe('aGVsbG8=')
  const large = new Uint8Array(200_000).fill(65)
  expect(atob(toBase64(large)).length).toBe(200_000)
})

test("the session's log names each staged file with its path, type and size", () => {
  const e: Event = {
    seq: 1,
    node_id: 'n',
    session_id: 's',
    kind: 'attachments_staged',
    ref_id: 'p1',
    payload: {
      dir: '.tracon/attachments/p1',
      attachments: [
        { name: 'shot.png', path: '.tracon/attachments/p1/shot.png', type: 'image/png', size: 2048, sha256: 'ab' },
        { name: 'run.log', path: '.tracon/attachments/p1/run.log', type: 'text/plain', size: 12, sha256: 'cd' },
      ],
    },
    at_ms: 0,
    mono_ms: 1,
  }
  expect(stagedOf(e.payload).length).toBe(2)
  expect(eventLine(e)?.text).toBe(
    'attached 2 files · .tracon/attachments/p1/shot.png (image/png, 2.0 KiB) · .tracon/attachments/p1/run.log (text/plain, 12 bytes)',
  )
})
