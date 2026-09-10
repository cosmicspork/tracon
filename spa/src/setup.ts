import { mount } from 'svelte'
import './app.css'
import Setup from './routes/Setup.svelte'

export default mount(Setup, { target: document.getElementById('app')! })
