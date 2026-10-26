// The router in this tab: the MIT core compiled to WebAssembly, run off the page's thread. It
// gets a board, its project, its custom rules and the router's options, posts every live frame
// back as it comes, and the routed board's text at the end. The page stops a run by terminating the worker.
import init, { last_panic, route } from '../router/router.js';

const ready = init();

self.onmessage = async ({ data }) => {
  try {
    await ready;
    const routed = route(data.pcb, data.pro, data.dru ?? '', data.options, data.fps, (line) => self.postMessage({ line }));
    self.postMessage({ done: routed });
  } catch (e) {
    const panic = last_panic?.();
    self.postMessage({ error: panic ? `the router panicked: ${panic}` : String(e?.message ?? e) });
  }
};
