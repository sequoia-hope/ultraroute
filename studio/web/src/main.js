import { mount } from 'svelte';
import init from './core/studio_core.js';
import App from './App.svelte';
import './app.css';

if (typeof window !== 'undefined' && !window.crossOriginIsolated && 'serviceWorker' in navigator) {
  navigator.serviceWorker.register('./coi-serviceworker.js').then((reg) => {
    reg.addEventListener('updatefound', () => {
      reg.installing?.addEventListener('statechange', function () {
        if (this.state === 'activated' && !sessionStorage.getItem('coiReloaded')) {
          sessionStorage.setItem('coiReloaded', '1');
          window.location.reload();
        }
      });
    });
  }).catch(() => {});
}

await init();
mount(App, { target: document.getElementById('app') });
