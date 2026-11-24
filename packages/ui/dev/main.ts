import '../index.css'
import { mount } from 'svelte'
import { connectTheme } from '../theme'
import Stories from './Stories.svelte'

connectTheme()
mount(Stories, { target: document.getElementById('app')! })
