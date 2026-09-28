// A textarea as tall as what it holds. Pass the bound value so a change made
// by code (a load, a reset) resizes it too, not only typing. CSS caps it with
// max-height, past which it scrolls.
export function autogrow(node: HTMLTextAreaElement, _value?: string) {
  const fit = () => {
    node.style.height = 'auto'
    node.style.height = `${node.scrollHeight + node.offsetHeight - node.clientHeight}px`
  }
  fit()
  node.addEventListener('input', fit)
  return {
    update: fit,
    destroy: () => node.removeEventListener('input', fit),
  }
}
