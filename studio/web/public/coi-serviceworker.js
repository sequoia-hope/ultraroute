// Cross-origin isolation on static hosts (such as GitHub Pages) that cannot set COOP/COEP
// response headers themselves: a service worker adds the headers to same-origin responses so
// SharedArrayBuffer and Atomics.wait are available to the in-tab router's worker pool and WebGPU
// bridge.
if (typeof window === 'undefined') {
  self.addEventListener('install', () => self.skipWaiting());
  self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));
  self.addEventListener('fetch', (event) => {
    const req = event.request;
    if (req.cache === 'only-if-cached' && req.mode !== 'same-origin') return;
    event.respondWith(
      fetch(req).then((res) => {
        if (res.status === 0) return res;
        const headers = new Headers(res.headers);
        headers.set('Cross-Origin-Opener-Policy', 'same-origin');
        headers.set('Cross-Origin-Embedder-Policy', 'require-corp');
        headers.set('Cross-Origin-Resource-Policy', 'same-origin');
        return new Response(res.body, {
          status: res.status,
          statusText: res.statusText,
          headers,
        });
      }),
    );
  });
} else if (!window.crossOriginIsolated && window.isSecureContext && 'serviceWorker' in navigator) {
  const scriptUrl = document.currentScript?.src || new URL('./coi-serviceworker.js', location.href).href;
  navigator.serviceWorker
    .register(scriptUrl)
    .then((reg) => {
      if (reg.active && !navigator.serviceWorker.controller) {
        if (!sessionStorage.getItem('coi-reloaded')) {
          sessionStorage.setItem('coi-reloaded', '1');
          location.reload();
        }
        return;
      }
      const watch = reg.installing || reg.waiting;
      if (watch) {
        watch.addEventListener('statechange', () => {
          if (watch.state === 'activated' && !sessionStorage.getItem('coi-reloaded')) {
            sessionStorage.setItem('coi-reloaded', '1');
            location.reload();
          }
        });
      }
    })
    .catch(() => {});
}
