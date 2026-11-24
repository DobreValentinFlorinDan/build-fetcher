// A full garbage collection in the calling thread, where the middleware allows it (server/index.ts exposes gc to the
// threads it starts). After work that leaves a lot behind (an evaluation step, a model rebuild), the heap goes back to
// what is alive instead of growing until V8 decides to collect: the evaluation's peak is about a third lower. Elsewhere
// (tests, npm scripts) it does nothing.
import v8 from 'node:v8';

export const collectGarbage = (thorough = false) => {
  const gc = (globalThis as { gc?: (o?: object) => void }).gc;
  if (!gc) return;
  // Thorough (0.9.1): V8's last-resort collection, which also compacts the heap and hands the pages a big piece of work
  // left empty back to the system. A regular one keeps them for later: after a model rebuild at 54,000 games the model
  // thread kept about 900 MB for 212 MB of live data, against about 400 MB this way. It takes about half a second there.
  if (thorough) gc({ type: 'major', execution: 'sync', flavor: 'last-resort' });
  else gc();
};

/** Lets the threads started after this collect garbage on purpose (the main thread itself can't). */
export function exposeGcToThreads() { v8.setFlagsFromString('--expose-gc'); }
