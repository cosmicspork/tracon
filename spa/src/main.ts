import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'

// Registered so the interface can be installed and opens fast. It caches the
// shell and nothing else; see public/sw.js. A browser that refuses (private
// mode, an insecure origin that is not localhost) simply runs without one.
if ('serviceWorker' in navigator) {
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('/sw.js').catch(() => {})
  })
}

export default mount(App, { target: document.getElementById('app')! })
