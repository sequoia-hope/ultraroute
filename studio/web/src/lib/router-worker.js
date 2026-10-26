// The router in this tab: the MIT core compiled to WebAssembly, run off the page's thread. It
// gets a board, its project, its custom rules and the router's options, posts every live frame
// back as it comes, and the routed board's text at the end. The page stops a run by terminating
// the worker. When the page is cross-origin isolated (SharedArrayBuffer and Atomics), it starts
// a pool of sub-workers, each with its own copy of the router, and negotiation's rounds are
// routed across them in batches.
import init, { last_panic, route_accelerated } from '../router/router.js';

const SLOTS_WORD_OFFSET = 64;
const SLOT_STRIDE = 4096;
const MAX_BATCH = 32;
// Connections handed to the pool at once; the result does not depend on it, only the speed.
const BATCH = 16;
const INPUT_WORD_OFFSET = SLOTS_WORD_OFFSET + MAX_BATCH * SLOT_STRIDE;
// A round's shared prices sit at INPUT_WORD_OFFSET, the batches priced from them after; the
// WebGPU path's batches, which carry every connection's prices, at INPUT_WORD_OFFSET instead.
const ROUND_WORDS = 12 * 1024 * 1024;
const JOBS_WORD_OFFSET = INPUT_WORD_OFFSET + ROUND_WORDS;
const SAB_BYTES = 128 * 1024 * 1024;
// A slot's length word for a path too long for it: the router routes that job itself.
const TOO_LONG = 0xffffffff;

const ready = init();
let poolPromise = null;

async function getWorkerPool(wantGpu) {
  if (typeof SharedArrayBuffer === 'undefined' || !self.crossOriginIsolated) {
    return null;
  }
  if (!poolPromise) {
    poolPromise = (async () => {
      try {
        const sab = new SharedArrayBuffer(SAB_BYTES);
        const ctrl = new Int32Array(sab, 0, 64);
        const u32 = new Uint32Array(sab);
        const hw = typeof navigator !== 'undefined' ? (navigator.hardwareConcurrency || 4) : 4;
        const numWorkers = Math.max(1, Math.min(8, hw));
        const workers = [];
        const readyPromises = [];
        for (let i = 0; i < numWorkers; i++) {
          const w = new Worker(new URL('./router-subworker.js', import.meta.url), { type: 'module' });
          readyPromises.push(
            new Promise((resolve, reject) => {
              const onMsg = ({ data }) => {
                if (data?.ready) {
                  w.removeEventListener('message', onMsg);
                  resolve(data);
                }
              };
              w.addEventListener('message', onMsg);
              w.addEventListener('error', reject, { once: true });
            })
          );
          w.postMessage({ sab, workerId: i, enableGpu: true });
          workers.push(w);
        }
        const statuses = await Promise.all(readyPromises);
        const hasGpu = statuses.some((s) => s.hasGpu);
        let epoch = 0;
        return {
          sab,
          ctrl,
          u32,
          workers,
          numWorkers,
          hasGpu,
          nextEpoch() {
            epoch = (epoch + 1) | 0;
            if (epoch <= 0) epoch = 1;
            return epoch;
          },
        };
      } catch (_) {
        return null;
      }
    })();
  }
  const pool = await poolPromise;
  if (!pool) return null;
  return pool;
}

self.onmessage = async ({ data }) => {
  try {
    await ready;
    const opts = String(data.options ?? '');
    const wantGpu = opts.split(/\s+/).includes('--gpu');
    const pool = await getWorkerPool(wantGpu);
    // Negotiation's rounds always go in batches, every connection priced as the round began, as
    // the native router does on several threads: the board comes out the same with a pool or
    // without one, on any number of cores, and the same as the studio's server routes it.
    // Without a pool the batches are routed here, one connection after another.
    if (!pool || (pool.numWorkers <= 1 && !(wantGpu && pool.hasGpu))) {
      const routed = route_accelerated(data.pcb, data.pro, data.dru ?? '', opts, data.fps, (line) => self.postMessage({ line }), BATCH, () => null, false);
      self.postMessage({ done: routed });
      return;
    }

    const useGpu = wantGpu && pool.hasGpu;
    const batchSize = useGpu ? 16 : BATCH;
    // Hands the pool a batch already in the buffer and waits for it; the paths, as the router
    // reads them.
    const runBatch = (numJobs, inputLen, mode) => {
      Atomics.store(pool.ctrl, 1, numJobs);
      Atomics.store(pool.ctrl, 2, 0);
      Atomics.store(pool.ctrl, 3, 0);
      Atomics.store(pool.ctrl, 4, 0);
      Atomics.store(pool.ctrl, 5, inputLen);
      Atomics.store(pool.ctrl, 6, mode);
      Atomics.store(pool.ctrl, 0, pool.nextEpoch());
      Atomics.notify(pool.ctrl, 0, mode === 1 ? 1 : pool.numWorkers);

      Atomics.wait(pool.ctrl, 4, 0);

      let totalWords = 0;
      for (let k = 0; k < numJobs; k++) {
        const len = pool.u32[SLOTS_WORD_OFFSET + k * SLOT_STRIDE];
        totalWords += len === TOO_LONG ? 1 : 1 + len;
      }
      const out = new Uint32Array(totalWords);
      let pos = 0;
      for (let k = 0; k < numJobs; k++) {
        const slotBase = SLOTS_WORD_OFFSET + k * SLOT_STRIDE;
        const len = pool.u32[slotBase];
        out[pos++] = len;
        if (len > 0 && len !== TOO_LONG) {
          out.set(pool.u32.subarray(slotBase + 1, slotBase + 1 + len), pos);
          pos += len;
        }
      }
      return out;
    };
    // The router's messages: a round's prices (1), kept for the workers to take once; a batch
    // priced from them (2); or, on the WebGPU path, a batch with every connection's prices.
    const dispatchBatch = (inputView) => {
      if (!useGpu && inputView[0] === 1) {
        if (inputView.length > ROUND_WORDS) return null;
        pool.u32.set(inputView, INPUT_WORD_OFFSET);
        Atomics.store(pool.ctrl, 8, inputView.length);
        Atomics.add(pool.ctrl, 7, 1);
        return new Uint32Array([1]);
      }
      if (!useGpu) {
        const numJobs = inputView[6];
        if (numJobs === 0 || numJobs > MAX_BATCH || JOBS_WORD_OFFSET + inputView.length > pool.u32.length) return null;
        pool.u32.set(inputView, JOBS_WORD_OFFSET);
        return runBatch(numJobs, inputView.length, 2);
      }
      const numJobs = inputView[0];
      if (numJobs === 0 || numJobs > MAX_BATCH) return null;
      if (INPUT_WORD_OFFSET + inputView.length > pool.u32.length) return null;
      pool.u32.set(inputView, INPUT_WORD_OFFSET);
      return runBatch(numJobs, inputView.length, 1);
    };

    const routed = route_accelerated(
      data.pcb,
      data.pro,
      data.dru ?? '',
      opts,
      data.fps,
      (line) => self.postMessage({ line }),
      batchSize,
      dispatchBatch,
      !useGpu
    );
    self.postMessage({ done: routed });
  } catch (e) {
    const panic = last_panic?.();
    self.postMessage({ error: panic ? `the router panicked: ${panic}` : String(e?.message ?? e) });
  }
};
