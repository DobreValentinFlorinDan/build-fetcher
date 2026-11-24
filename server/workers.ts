import { fork, type ChildProcess } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { Worker, type Transferable } from 'node:worker_threads';

// The app's worker threads. Source runs (tsx) load the TypeScript entry through worker-dev.mjs, which registers tsx in
// the worker thread; the bundled build (scripts/bundle.mjs) emits one .mjs per worker next to index.mjs.
//   scraper  server/scraper/worker.ts   every Riot API call, the cache and the database writes it owns
//   model    server/model/worker.ts     the lane baseline, the draft model, live "Next" and the personal layer
//   prepare  server/model/prepareWorker.ts  a model build's games prepared on several threads (0.15.0)
//   job      server/jobWorker.ts        one-shot jobs: npm run eval:builds and the data audit in the app
//   eval     server/model/evalWorker.ts an evaluation's other threads (0.11.0), started by the job thread
//   gpu      server/gpu/worker.ts       Test GPU and the clean-up after a stopped evaluation (0.13.0)
//   convert  server/convertWorker.ts    the background conversion of earlier versions' data (0.14.0)
//   slim     server/slimWorker.ts       Slim stored data: backup, baseline, slimming, verification (0.14.0)
//   slimPart server/slimPartWorker.ts   its slimming's other threads (0.14.4), started by the slim thread
//   transfer server/transferWorker.ts   pool backup (ZIP export) and import (0.19.0)
const ENTRIES = {
  scraper: { source: './scraper/worker.ts', bundled: './worker.mjs' },
  model: { source: './model/worker.ts', bundled: './model-worker.mjs' },
  prepare: { source: './model/prepareWorker.ts', bundled: './prepare-worker.mjs' },
  job: { source: './jobWorker.ts', bundled: './job-worker.mjs' },
  eval: { source: './model/evalWorker.ts', bundled: './eval-worker.mjs' },
  gpu: { source: './gpu/worker.ts', bundled: './gpu-worker.mjs' },
  convert: { source: './convertWorker.ts', bundled: './convert-worker.mjs' },
  slim: { source: './slimWorker.ts', bundled: './slim-worker.mjs' },
  slimPart: { source: './slimPartWorker.ts', bundled: './slim-part-worker.mjs' },
  transfer: { source: './transferWorker.ts', bundled: './transfer-worker.mjs' },
} as const;

export type WorkerName = keyof typeof ENTRIES;

const fromSource = import.meta.url.endsWith('.ts');

/**
 * `transferList`: what moves to the thread instead of being copied (a MessagePort to talk on). `heapMb`: the most heap
 * the thread may hold; past it the thread stops ("out of memory") and nothing else does.
 */
export function startWorker(name: WorkerName, workerData: object, transferList: Transferable[] = [], heapMb?: number): Worker {
  const entry = ENTRIES[name];
  const resourceLimits = heapMb ? { maxOldGenerationSizeMb: heapMb } : undefined;
  return fromSource
    ? new Worker(new URL('./worker-dev.mjs', import.meta.url), { workerData: { ...workerData, entry: new URL(entry.source, import.meta.url).href }, transferList, resourceLimits })
    : new Worker(new URL(entry.bundled, import.meta.url), { workerData, transferList, resourceLimits });
}

/**
 * The app's child processes (0.22.0): an evaluation's members (model/evalProcess.ts), each with a JavaScript heap of its
 * own instead of a share of the app's (model/evalTeam.ts). In the desktop app they run the app's own executable as
 * Node (ELECTRON_RUN_AS_NODE); from source, Node with tsx. `heapMb`: the most heap the process may hold.
 */
const PROCESSES = {
  eval: { source: './model/evalProcess.ts', bundled: './eval-process.mjs' },
  // The job thread's program run as a process instead (0.22.0, server/jobs.ts): the evaluation, out of the app's heap.
  job: { source: './jobWorker.ts', bundled: './job-worker.mjs' },
} as const;

export function startProcess(name: keyof typeof PROCESSES, heapMb?: number): ChildProcess {
  const entry = PROCESSES[name];
  const file = fileURLToPath(new URL(fromSource ? entry.source : entry.bundled, import.meta.url));
  const execArgv = [...(fromSource ? ['--import', 'tsx'] : []), '--expose-gc', ...(heapMb ? [`--max-old-space-size=${heapMb}`] : [])];
  return fork(file, [], {
    execArgv, serialization: 'advanced', windowsHide: true, stdio: ['ignore', 'inherit', 'inherit', 'ipc'],
    env: { ...process.env, ...(process.versions.electron ? { ELECTRON_RUN_AS_NODE: '1' } : {}) },
  });
}

