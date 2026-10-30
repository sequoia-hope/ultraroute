// Sub-worker for parallel WASM A* routing: a batch of a negotiation round's connections, each
// priced from the round's shared prices.
// Communicates with router-worker.js through a SharedArrayBuffer + Atomics.
import init, { route_packed_job, route_priced_job, set_round } from '../router/router.js';

const SLOTS_WORD_OFFSET = 64;
const SLOT_STRIDE = 4096;
// Written in a slot's length word when a path does not fit the slot: the router routes that
// job itself.
const TOO_LONG = 0xffffffff;
const INPUT_WORD_OFFSET = SLOTS_WORD_OFFSET + 32 * SLOT_STRIDE;
// Where router-worker.js puts a round's shared prices (at INPUT_WORD_OFFSET) and the batches
// priced from them.
const ROUND_WORDS = 12 * 1024 * 1024;
const JOBS_WORD_OFFSET = INPUT_WORD_OFFSET + ROUND_WORDS;

const wasmReady = init();

self.onmessage = async ({ data }) => {
  await wasmReady;
  const { sab, workerId } = data;
  const ctrl = new Int32Array(sab, 0, 64);
  const u32 = new Uint32Array(sab);
  self.postMessage({ ready: true, workerId });

  let lastEpoch = 0;
  let heldRound = -1;
  while (true) {
    const waitRes = Atomics.wait(ctrl, 0, lastEpoch);
    if (waitRes === 'ok' || waitRes === 'not-equal') {
      const epoch = Atomics.load(ctrl, 0);
      if (epoch < 0) break;
      if (epoch === lastEpoch) continue;
      lastEpoch = epoch;

      const numJobs = Atomics.load(ctrl, 1);
      const inputLen = Atomics.load(ctrl, 5);
      const mode = Atomics.load(ctrl, 6);

      if (mode === 2) {
        // A batch priced from the round's shared prices, taken once a round.
        const round = Atomics.load(ctrl, 7);
        if (round !== heldRound) {
          set_round(u32.subarray(INPUT_WORD_OFFSET, INPUT_WORD_OFFSET + Atomics.load(ctrl, 8)));
          heldRound = round;
        }
        const jobs = u32.subarray(JOBS_WORD_OFFSET, JOBS_WORD_OFFSET + inputLen);
        const header = jobs.subarray(0, 7);
        while (true) {
          const k = Atomics.add(ctrl, 2, 1);
          if (k >= numJobs) break;
          const end = k + 1 < numJobs ? jobs[7 + k + 1] : inputLen;
          const path = route_priced_job(header, jobs.subarray(jobs[7 + k], end));
          const slotBase = SLOTS_WORD_OFFSET + k * SLOT_STRIDE;
          if (path.length > SLOT_STRIDE) {
            u32[slotBase] = TOO_LONG;
          } else {
            u32.set(path, slotBase);
          }
          if (Atomics.add(ctrl, 3, 1) + 1 === numJobs) {
            Atomics.store(ctrl, 4, 1);
            Atomics.notify(ctrl, 4, 1);
          }
        }
        continue;
      }

      // A batch carrying every connection's prices.
      const words = u32.subarray(INPUT_WORD_OFFSET, INPUT_WORD_OFFSET + inputLen);

      while (true) {
        const k = Atomics.add(ctrl, 2, 1);
        if (k >= numJobs) break;
        const path = route_packed_job(words, k);
        const slotBase = SLOTS_WORD_OFFSET + k * SLOT_STRIDE;
        if (path.length > SLOT_STRIDE) {
          u32[slotBase] = TOO_LONG;
        } else {
          u32.set(path, slotBase);
        }
        if (Atomics.add(ctrl, 3, 1) + 1 === numJobs) {
          Atomics.store(ctrl, 4, 1);
          Atomics.notify(ctrl, 4, 1);
        }
      }
    }
  }
};
