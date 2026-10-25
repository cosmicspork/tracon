// A textarea as tall as what it holds, up to its CSS max-height (app.css's
// `textarea.grow`: the visible page below its header), past which it scrolls
// inside. Pass the bound value so a change made by code (a load, a reset,
// "take theirs") resizes it too, not only typing; a window resize moves the
// cap and the wrap, so it refits then as well.
export function autogrow(node: HTMLTextAreaElement, _value?: string) {
  const view = node.ownerDocument.defaultView
  const fit = () => {
    node.style.height = 'auto'
    const full = node.scrollHeight + node.offsetHeight - node.clientHeight
    // `none` parses to NaN: no cap, so the whole of it.
    const cap = parseFloat(view?.getComputedStyle(node).maxHeight ?? '')
    node.style.height = `${Number.isFinite(cap) ? Math.min(full, cap) : full}px`
  }
  fit()
  node.addEventListener('input', fit)
  view?.addEventListener('resize', fit)
  return {
    update: (_value?: string) => fit(),
    destroy: () => {
      node.removeEventListener('input', fit)
      view?.removeEventListener('resize', fit)
    },
  }
}
