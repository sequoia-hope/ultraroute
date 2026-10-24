import { mount } from 'svelte';
import init from './core/studio_core.js';
import App from './App.svelte';
import './app.css';

await init();
mount(App, { target: document.getElementById('app') });
