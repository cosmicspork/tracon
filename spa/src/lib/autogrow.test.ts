import { expect, test } from 'bun:test'

import { autogrow } from './autogrow'

// A textarea with no layout engine behind it: its content is `content` pixels
// tall, it has a 1px border top and bottom, and its computed max-height is
// `maxHeight`. Listeners on it and on its window are kept so a test can fire
// them.
function stub(content: number, maxHeight = 'none') {
  const windowListeners = new Map<string, () => void>()
  const view = {
    getComputedStyle: () => ({ maxHeight: box.maxHeight }),
    addEventListener: (type: string, f: () => void) => windowListeners.set(type, f),
    removeEventListener: (type: string) => windowListeners.delete(type),
  }
  const listeners = new Map<string, () => void>()
  const box = {
    content,
    maxHeight,
    style: { height: '' },
    get scrollHeight() {
      return this.content
    },
    offsetHeight: 2,
    clientHeight: 0,
    ownerDocument: { defaultView: view },
    addEventListener: (type: string, f: () => void) => listeners.set(type, f),
    removeEventListener: (type: string) => listeners.delete(type),
  }
  const node = box as unknown as HTMLTextAreaElement
  return { box, node, listeners, windowListeners }
}

test('a box opens as tall as what it holds, borders included', () => {
  const { node } = stub(600)
  autogrow(node, 'forty lines')
  expect(node.style.height).toBe('602px')
})

test('a box past its max-height stops there and scrolls inside', () => {
  const { node } = stub(6000, '700px')
  autogrow(node, 'four hundred lines')
  expect(node.style.height).toBe('700px')
})

test('typing refits the box', () => {
  const { box, node, listeners } = stub(100)
  autogrow(node, '')
  box.content = 300
  listeners.get('input')!()
  expect(node.style.height).toBe('302px')
})

test('a value set by code refits the box', () => {
  const { box, node } = stub(100)
  const action = autogrow(node, 'mine')
  box.content = 400
  action.update('theirs')
  expect(node.style.height).toBe('402px')
})

test('a window resize refits the box against the new cap', () => {
  const { box, node, windowListeners } = stub(900, '800px')
  autogrow(node, 'long')
  expect(node.style.height).toBe('800px')
  box.maxHeight = '500px'
  windowListeners.get('resize')!()
  expect(node.style.height).toBe('500px')
})

test('a destroyed box stops listening to typing and to the window', () => {
  const { node, listeners, windowListeners } = stub(100)
  autogrow(node, '').destroy()
  expect(listeners.size).toBe(0)
  expect(windowListeners.size).toBe(0)
})

test('measuring keeps the page where it was scrolled', () => {
  const { box, node } = stub(600)
  // A layout engine that clamps the page's scroll while the box is collapsed.
  const page = { scrollTop: 1880 }
  let height = ''
  Object.assign(box, {
    ownerDocument: { ...box.ownerDocument, scrollingElement: page },
    style: {
      get height() {
        return height
      },
      set height(v: string) {
        height = v
        if (v === 'auto') page.scrollTop = 1300
      },
    },
  })
  autogrow(node)
  expect(node.style.height).toBe('602px')
  expect(page.scrollTop).toBe(1880)
})
