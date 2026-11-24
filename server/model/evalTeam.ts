// Evaluate builds on every core (0.11.0). The evaluation's time is its loops over held-out games, each game on its own
// (server/model/evaluate.ts): a team of threads shares them out and puts the results back in order, so the report is the
// same to the last digit as on one thread.
//
// Every thread runs the same program (rank 0 is the job thread, the others its eval workers, model/evalWorker.ts) from
// the same data, copied to each. At each loop (`forEach`) the threads take chunks of games from a shared counter
// (whoever is free takes the next), rank 0 gathers every chunk's results and applies them in game order: the same sums
// in the same order as one thread. Only rank 0 keeps results (0.12.0): the few choices that steer later work (the
// bandwidth and prior, the fade per patch, the expected win rates) it shares with the others (`share`), so they hold no
// outcomes of their own.
//
// How many threads: the computer's logical cores, as many as its available memory holds with a reserve left free, so
// nothing spills to disk (planThreads). Champ select or a game stops the workers, so their memory goes back to the game,
// and rank 0 finishes alone. A worker that dies (or is stopped because memory ran low) leaves its games to rank 0.
import os from 'node:os';
import v8 from 'node:v8';
import { MessageChannel, type MessagePort, type Worker } from 'node:worker_threads';

export interface Team {
  /** 0: the thread whose report is kept. */
  readonly rank: number;
  /** Threads started (the team's chunks are sized for them). */
  readonly size: number;
  /**
   * Item j = 0..n-1: `compute(j)` somewhere in the team, then `apply(j, result)` for every j in order, in rank 0 only.
   * `compute` must depend on nothing `apply` changes (the evaluation's per-game work); its result crosses threads.
   * `progress(done, of)`: in rank 0, now and then.
   */
  forEach<R>(n: number, compute: (j: number) => R, apply: (j: number, r: R) => void, progress?: (done: number, of: number) => void): Promise<void>;
  /**
   * Rank 0's `value()`, in every thread: a choice made from results only rank 0 holds (the others don't call `value`).
   * Every thread calls it at the same point of the program.
   */
  share<T>(value: () => T): Promise<T>;
  /** Threads working now (fewer than `size` once some stopped). */
  working(): number;
  /** The most memory one of the other threads held (its heap and buffers), bytes; 0 without any. */
  peak(): number;
  /** ...and the most heap alone (0.19.0: what counts against a heap the threads share), bytes; 0 without any. */
  heapPeak(): number;
  close(): Promise<void>;
}

/** One thread: each item computed and applied in turn. */
export const solo: Team = {
  rank: 0, size: 1,
  async forEach(n, compute, apply, progress) {
    for (let j = 0; j < n; j++) {
      progress?.(j, n);
      apply(j, compute(j));
    }
  },
  async share(value) { return value(); },
  working: () => 1,
  peak: () => 0,
  heapPeak: () => 0,
  async close() {},
};

/** Loops one evaluation runs at most (a counter each), and chunks one loop has at most. */
const MAX_LOOPS = 4096;
const MAX_CHUNKS = 1 << 16;

/**
 * Items per chunk: small enough that the threads finish together, large enough that messages stay few (and no more
 * chunks than the shared table holds).
 */
export const chunkOf = (n: number, size: number) => Math.max(1, Math.min(32, Math.ceil(n / (size * 24))), Math.ceil(n / MAX_CHUNKS));

/** What the threads share: a counter per loop, which thread took each chunk, and the app's "busy" flag. */
export interface TeamMemory { counters: SharedArrayBuffer; owners: SharedArrayBuffer; control: SharedArrayBuffer | null }

export const teamMemory = (control: SharedArrayBuffer | null): TeamMemory => ({
  counters: new SharedArrayBuffer(MAX_LOOPS * 4),
  owners: new SharedArrayBuffer(MAX_CHUNKS * 4),
  control,
});

/**
 * The app's control flags (server/jobs.ts): [0] = 1 while champ select or a game is on (the workers stop); [1] the
 * heap the middleware's other threads hold (main, model, scraper; MB, 0: not measured), kept by the main thread while
 * an evaluation runs (0.15.1).
 */
export const controlFlags = () => new SharedArrayBuffer(8);
const busy = (control: Int32Array | null) => !!control && Atomics.load(control, 0) === 1;
/** Sets the busy flag (the main thread, as champ select or a game starts and ends). */
export const setBusyFlag = (control: SharedArrayBuffer, on: boolean) => Atomics.store(new Int32Array(control), 0, on ? 1 : 0);
/** Records what the other threads hold of the shared heap (bytes). */
export const setOtherHeap = (control: SharedArrayBuffer, bytes: number) => Atomics.store(new Int32Array(control), 1, Math.ceil(bytes / 1024 ** 2));

type ChunkMessage = { type: 'chunk'; loop: number; start: number; results: unknown[] } | { type: 'peak'; bytes: number; heap: number };
/** From rank 0: a loop is over (every chunk in), and a shared value. */
type LeadMessage = { type: 'done'; loop: number } | { type: 'share'; id: number; value: unknown };

const yieldNow = () => new Promise((r) => setImmediate(r));

/**
 * A value serialised once into memory every thread can read (each deserialises its own copy, v8.deserialize): not
 * copied once per thread on its way there.
 */
export function shareValue(value: unknown): SharedArrayBuffer {
  const bytes = v8.serialize(value);
  const blob = new SharedArrayBuffer(bytes.length);
  new Uint8Array(blob).set(bytes);
  return blob;
}

/** Memory the system can still give without swapping (cgroup limits included where Node knows them). */
export function availableMemory(): number {
  const p = process as { availableMemory?: () => number };
  const free = os.freemem();
  const own = typeof p.availableMemory === 'function' ? p.availableMemory() : free;
  return Math.min(free, own > 0 ? own : free);
}

/** Left free whatever the evaluation does: the game, Windows and the rest of the app need it. */
export const memoryReserve = (total = os.totalmem()) => Math.max(1.5 * 1024 ** 3, 0.15 * total);

/** Logical cores (threads the processor runs at once). */
export const cores = () => (typeof os.availableParallelism === 'function' ? os.availableParallelism() : os.cpus().length);

/**
 * The JavaScript heap all of this process's threads share, where they share one (bytes; null: each thread has its own).
 * In the desktop app (Electron, 0.12.1) every thread's heap lives in one 4 GB pointer-compression cage: the middleware,
 * the model and scraper threads and every evaluation thread together. Past it a thread dies "out of memory" whatever
 * the computer's free memory.
 */
export function sharedHeapCeiling(): number | null {
  return process.versions.electron ? 4 * 1024 ** 3 : null;
}

/** What the middleware's other threads (main, model, scraper) may hold of a shared heap, at least. */
export const OTHER_THREADS_HEAP = 1024 ** 3;

/**
 * What the other threads may hold of a shared heap while an evaluation runs (0.15.1): what they hold now (setOtherHeap)
 * and a quarter more, plus 256 MB (the lane builds' games, read when a build is asked for: about 130 MB at 54,000
 * games), at least OTHER_THREADS_HEAP. At 118,000 games the model thread alone may hold about 0.8 GB (an estimate);
 * no model build starts while an evaluation runs (model/worker.ts), so it doesn't double meanwhile.
 */
export function otherThreadsHeap(control: SharedArrayBuffer | null): number {
  const mb = control && control.byteLength >= 8 ? Atomics.load(new Int32Array(control), 1) : 0;
  return Math.max(OTHER_THREADS_HEAP, mb > 0 ? mb * 1024 ** 2 * 1.25 + 256 * 1024 ** 2 : 0);
}

/**
 * Threads for an evaluation: every logical core, as long as each one's memory (`perThread`, bytes) fits in what is
 * available now beyond the reserve and, where the threads share one heap (`heap`), in what is left of it beyond what
 * the rest of the process holds (`heap.used`); at least 1 (the job thread, already running).
 */
export function planThreads(perThread: number, o: {
  available?: number; total?: number; cores?: number; max?: number;
  /** A heap the threads share (0.12.1), what the rest holds of it, and what each thread takes of it (0.19.0; else perThread). */
  heap?: { ceiling: number; used: number; each?: number };
} = {}): number {
  const available = o.available ?? availableMemory();
  const spare = available - memoryReserve(o.total);
  const each = Math.max(1, perThread);
  const byMemory = 1 + Math.max(0, Math.floor(spare / each));
  const byHeap = o.heap ? 1 + Math.max(0, Math.floor((o.heap.ceiling - o.heap.used) / Math.max(1, o.heap.each ?? each))) : Infinity;
  return Math.max(1, Math.min(o.cores ?? cores(), o.max ?? Infinity, byMemory, byHeap));
}

/**
 * Rank 0's side: the workers started (`spawn(rank, port, memory)` starts one), the chunks they send, and the results
 * sent back. `minFree`: below this much available memory, a worker stops at the next loop and leaves its share here.
 */
export async function leadTeam(size: number, spawn: (rank: number, port: MessagePort, memory: TeamMemory) => Worker,
  control: SharedArrayBuffer | null, o: { minFree?: number } = {}): Promise<Team> {
  const memory = teamMemory(control);
  const counters = new Int32Array(memory.counters);
  const owners = new Int32Array(memory.owners);
  owners.fill(-1);
  type Member = { rank: number; worker: Worker; port: MessagePort; alive: boolean };
  const members: Member[] = [];
  let peak = 0, heapPeak = 0;
  const alive = (rank: number) => members.some((m) => m.alive && m.rank === rank);
  const flags = control ? new Int32Array(control) : null;
  /** Stops a worker: what it took of the loop running now is worked out here (recover). */
  const stop = (m: Member) => { m.alive = false; void m.worker.terminate(); };
  /** Champ select or a game: every worker stops, so its memory goes back to the game. */
  const yieldToGame = () => {
    if (!busy(flags) || !members.some((m) => m.alive)) return;
    console.warn('[eval] champ select or a game: the other threads stop, this one finishes');
    for (const m of members) if (m.alive) stop(m);
  };
  /** The loop running now: its results so far, which chunks are in, and how to work one out here. */
  type Loop = { loop: number; n: number; chunk: number; results: unknown[]; done: Uint8Array; left: number; items: number; compute: (j: number) => unknown; wake: (() => void) | null };
  let current: Loop | null = null;

  const take = (c: Loop, start: number, results: unknown[]) => {
    const k = start / c.chunk;
    if (c.done[k]) return;   // in already (worked out here after its worker stopped): the same values
    c.done[k] = 1;
    for (let i = 0; i < results.length; i++) c.results[start + i] = results[i];
    c.left--;
    c.items += results.length;
    if (!c.left) c.wake?.();
  };
  const here = (c: Loop, start: number) => {
    const out: unknown[] = [];
    for (let j = start; j < Math.min(c.n, start + c.chunk); j++) out.push(c.compute(j));
    take(c, start, out);
  };
  /**
   * Chunks taken by a worker that stopped are worked out here. A chunk whose taker hasn't written its rank yet (the
   * moment between taking and marking) waits one more check before it counts as lost.
   */
  let suspects = new Set<number>();
  const recover = (c: Loop) => {
    const claimed = Math.min(c.n, Atomics.load(counters, c.loop));
    const next = new Set<number>();
    for (let start = 0; start < claimed; start += c.chunk) {
      const k = start / c.chunk;
      if (c.done[k]) continue;
      const owner = Atomics.load(owners, k);
      if (owner > 0 && alive(owner)) continue;
      if (owner === -1 && !suspects.has(k)) { next.add(k); continue; }
      here(c, start);
    }
    suspects = next;
  };

  for (let rank = 1; rank < size; rank++) {
    const { port1, port2 } = new MessageChannel();
    const worker = spawn(rank, port2, memory);
    const m: Member = { rank, worker, port: port1, alive: true };
    port1.on('message', (msg: ChunkMessage) => {
      if (msg.type === 'peak') { peak = Math.max(peak, msg.bytes); heapPeak = Math.max(heapPeak, msg.heap); }
      else if (msg.type === 'chunk' && current && msg.loop === current.loop) take(current, msg.start, msg.results);
    });
    worker.on('error', (err) => console.warn(`[eval] thread ${rank} failed:`, err));
    worker.on('exit', () => { m.alive = false; current?.wake?.(); });
    members.push(m);
  }
  let loop = 0, shared = 0;

  return {
    rank: 0, size,
    working: () => 1 + members.filter((m) => m.alive).length,
    peak: () => peak,
    heapPeak: () => heapPeak,
    async forEach(n, compute, apply, progress) {
      const id = loop++;
      const chunk = chunkOf(n, size);
      const chunks = Math.ceil(n / chunk);
      const c: Loop = current = { loop: id, n, chunk, results: new Array(n), done: new Uint8Array(chunks), left: chunks, items: 0, compute, wake: null };
      suspects = new Set();
      // Memory running low: one worker fewer from here on (what it took of this loop is worked out here).
      if (o.minFree !== undefined && availableMemory() < o.minFree) {
        const last = [...members].reverse().find((m) => m.alive);
        if (last) { console.warn(`[eval] memory is low: thread ${last.rank} stops`); stop(last); }
      }
      yieldToGame();
      let lastYield = Date.now(), lastProgress = 0;
      for (;;) {
        const start = Atomics.add(counters, id, chunk);
        if (start >= n) break;
        Atomics.store(owners, start / chunk, 0);
        here(c, start);
        const now = Date.now();
        if (progress && now - lastProgress > 250) { lastProgress = now; progress(c.items, n); }
        // The workers' chunks are taken in between ours.
        if (now - lastYield > 100) { lastYield = now; yieldToGame(); await yieldNow(); }
      }
      while (c.left) {
        await new Promise<void>((r) => { c.wake = r; setTimeout(r, 200); });
        c.wake = null;
        yieldToGame();
        recover(c);
        progress?.(c.items, n);
      }
      current = null;
      owners.fill(-1, 0, chunks);
      // The others go on once every chunk is in (they may take the next loop's chunks only after the table is reset).
      const done: LeadMessage = { type: 'done', loop: id };
      for (const m of members) if (m.alive) m.port.postMessage(done);
      for (let j = 0; j < n; j++) apply(j, c.results[j] as never);
    },
    async share(value) {
      const v = value();
      const msg: LeadMessage = { type: 'share', id: shared++, value: v };
      for (const m of members) if (m.alive) m.port.postMessage(msg);
      return v;
    },
    async close() {
      await Promise.all(members.map(async (m) => { m.alive = false; m.port.close(); await m.worker.terminate(); }));
    },
  };
}

/**
 * A worker's side (rank ≥ 1): it takes chunks while the app isn't busy, sends their results, and goes on once rank 0
 * has them all. It applies nothing: rank 0 shares what steers later work.
 */
export function joinTeam(rank: number, size: number, port: MessagePort, memory: TeamMemory): Team {
  const counters = new Int32Array(memory.counters);
  const owners = new Int32Array(memory.owners);
  const control = memory.control ? new Int32Array(memory.control) : null;
  /** Rank 0's messages by key (`done:<loop>`, `share:<id>`), and who waits for one. */
  const arrived = new Map<string, unknown>();
  const waiting = new Map<string, (v: unknown) => void>();
  port.on('message', (msg: LeadMessage) => {
    const key = msg.type === 'done' ? `done:${msg.loop}` : `share:${msg.id}`;
    const value = msg.type === 'share' ? msg.value : null;
    const w = waiting.get(key);
    if (w) { waiting.delete(key); w(value); } else arrived.set(key, value);
  });
  const next = (key: string) => {
    if (arrived.has(key)) { const v = arrived.get(key); arrived.delete(key); return Promise.resolve(v); }
    return new Promise<unknown>((r) => waiting.set(key, r));
  };
  let loop = 0, shared = 0, peak = 0, heapPeak = 0;
  /** This thread's memory: its heap and the buffers it holds (what another thread like it will need), and its heap. */
  const measure = () => {
    const h = v8.getHeapStatistics();
    peak = Math.max(peak, h.total_physical_size + h.external_memory);
    heapPeak = Math.max(heapPeak, h.total_heap_size);
  };
  return {
    rank, size,
    working: () => size,
    peak: () => peak,
    heapPeak: () => heapPeak,
    async forEach(n, compute) {
      const id = loop++;
      const chunk = chunkOf(n, size);
      for (;;) {
        // Champ select or a game: no more games taken (rank 0 stops this thread and carries on alone).
        if (busy(control)) break;
        const start = Atomics.add(counters, id, chunk);
        if (start >= n) break;
        Atomics.store(owners, start / chunk, rank);
        const results: unknown[] = [];
        for (let j = start; j < Math.min(n, start + chunk); j++) results.push(compute(j));
        port.postMessage({ type: 'chunk', loop: id, start, results } satisfies ChunkMessage);
        measure();
        // Between chunks, rank 0's results of earlier loops (none here) and the busy flag stay current.
        await yieldNow();
      }
      await next(`done:${id}`);
      measure();
      port.postMessage({ type: 'peak', bytes: peak, heap: heapPeak } satisfies ChunkMessage);
    },
    async share<T>() { return await next(`share:${shared++}`) as T; },
    async close() { port.close(); },
  };
}

// ---- A team of processes (0.22.0) ----------------------------------------------------------------------------------
// In the desktop app every thread's JavaScript heap lives in one 4 GB block (sharedHeapCeiling): the model thread, the
// scraper and every evaluation thread together, so at 170,000 games one or two evaluation threads fit, whatever the
// computer. A process has a 4 GB block of its own: rank 0 (the job thread) leads members that are processes instead,
// as many as the cores and the free memory allow. Nothing is shared between processes, so rank 0 hands out the chunks
// itself over each process's message channel (a member asks for the next chunk with the results of the last, and holds
// one more in hand so it never waits), and keeps track of who has which: a process that ends leaves its chunks to rank 0,
// as a thread does. The results are applied in game order in rank 0, so the report is the same as on one thread.

/** What rank 0 needs of a member process: Node's ChildProcess has it. */
export interface MemberProcess {
  send(msg: unknown): boolean;
  /** Whether its message channel is open (false once it ended). */
  readonly connected?: boolean;
  on(event: 'message', fn: (msg: unknown) => void): unknown;
  on(event: 'exit', fn: () => void): unknown;
  on(event: 'error', fn: (err: Error) => void): unknown;
  kill(): boolean;
}

/** From a member process: ready (its copy read), asking for chunks of a loop (`want` of them, with a chunk's results), its memory. */
export type MemberMessage =
  | { type: 'ready' }
  | { type: 'want'; loop: number; count: number }
  | { type: 'chunk'; loop: number; start: number; results: unknown[] }
  | { type: 'peak'; bytes: number; heap: number };
/** To a member process: a chunk to work out (start −1: none left in this loop), a loop over, a shared value. */
export type LeaderMessage =
  | { type: 'take'; loop: number; start: number }
  | { type: 'done'; loop: number }
  | { type: 'share'; id: number; value: unknown };

/**
 * Rank 0's side with member processes (`spawn(rank)` starts one; it says `ready` once it has read its copy). Members
 * that don't say ready within `readyMs` are stopped and their share is worked out here. `minFree`: as leadTeam.
 */
export async function leadProcessTeam(size: number, spawn: (rank: number) => MemberProcess, control: SharedArrayBuffer | null,
  o: { minFree?: number; readyMs?: number } = {}): Promise<Team> {
  const flags = control ? new Int32Array(control) : null;
  type Member = { rank: number; proc: MemberProcess; alive: boolean; ready: boolean };
  const members: Member[] = [];
  let peak = 0, heapPeak = 0;
  /** The loop running now in rank 0, and what members asked of loops rank 0 hasn't reached yet. */
  type Loop = {
    loop: number; n: number; chunk: number; next: number; results: unknown[]; done: Uint8Array; left: number; items: number;
    /** Who holds each chunk handed out and not yet in: chunk index → rank. */
    held: Map<number, number>; compute: (j: number) => unknown; wake: (() => void) | null;
  };
  let current: Loop | null = null;
  const early = new Map<number, Map<number, number>>();   // loop → rank → chunks wanted
  let finishedLoops = -1;

  const stop = (m: Member) => { if (!m.alive) return; m.alive = false; try { m.proc.kill(); } catch { /* gone */ } current?.wake?.(); };
  /**
   * A message to a member, while it is there: one that ended between its last message and this one is stopped (what
   * it held is worked out here) instead of the send failing.
   */
  const tell = (m: Member, msg: LeaderMessage) => {
    if (!m.alive) return false;
    if (m.proc.connected === false) { stop(m); return false; }
    try { m.proc.send(msg); return true; } catch { stop(m); return false; }
  };
  const yieldToGame = () => {
    if (!busy(flags) || !members.some((m) => m.alive)) return;
    console.warn('[eval] champ select or a game: the other processes stop, this thread finishes');
    for (const m of members) stop(m);
  };
  const take = (c: Loop, start: number, results: unknown[]) => {
    const k = start / c.chunk;
    if (c.done[k]) return;
    c.done[k] = 1;
    c.held.delete(k);
    for (let i = 0; i < results.length; i++) c.results[start + i] = results[i];
    c.left--;
    c.items += results.length;
    if (!c.left) c.wake?.();
  };
  /** The next chunk of loop `c` to `m` (or none left). */
  const handOut = (c: Loop, m: Member, count: number) => {
    for (let i = 0; i < count; i++) {
      if (!m.alive) return;
      if (c.next >= c.n) { tell(m, { type: 'take', loop: c.loop, start: -1 }); return; }
      const start = c.next;
      c.next += c.chunk;
      // Held before it is sent: if the send fails, rank 0 works it out.
      c.held.set(start / c.chunk, m.rank);
      if (!tell(m, { type: 'take', loop: c.loop, start })) return;
    }
  };
  const want = (m: Member, loop: number, count: number) => {
    if (loop <= finishedLoops) { tell(m, { type: 'take', loop, start: -1 }); return; }
    if (current && current.loop === loop) { handOut(current, m, count); return; }
    let byRank = early.get(loop);
    if (!byRank) { byRank = new Map(); early.set(loop, byRank); }
    byRank.set(m.rank, (byRank.get(m.rank) ?? 0) + count);
  };

  const readyAll: Promise<void>[] = [];
  for (let rank = 1; rank < size; rank++) {
    let proc: MemberProcess;
    try { proc = spawn(rank); } catch (err) { console.warn(`[eval] process ${rank} didn't start:`, err); continue; }
    const m: Member = { rank, proc, alive: true, ready: false };
    members.push(m);
    readyAll.push(new Promise<void>((resolve) => {
      const timer = setTimeout(() => { if (!m.ready) { console.warn(`[eval] process ${rank} wasn't ready in time: stopped`); stop(m); } resolve(); }, o.readyMs ?? 120_000);
      proc.on('message', (raw) => {
        const msg = raw as MemberMessage;
        if (msg.type === 'ready') { m.ready = true; clearTimeout(timer); resolve(); }
        else if (msg.type === 'peak') { peak = Math.max(peak, msg.bytes); heapPeak = Math.max(heapPeak, msg.heap); }
        else if (msg.type === 'want') want(m, msg.loop, msg.count);
        else if (msg.type === 'chunk' && current && msg.loop === current.loop) { take(current, msg.start, msg.results); handOut(current, m, 1); }
      });
      proc.on('exit', () => { m.alive = false; clearTimeout(timer); resolve(); current?.wake?.(); });
      proc.on('error', (err) => { console.warn(`[eval] process ${rank} failed:`, err); stop(m); });
    }));
  }
  // Each has read its copy before the first loop (a slow one is stopped rather than waited for).
  await Promise.all(readyAll);
  let loop = 0, shared = 0;

  return {
    rank: 0, size,
    working: () => 1 + members.filter((m) => m.alive).length,
    peak: () => peak,
    heapPeak: () => heapPeak,
    async forEach(n, compute, apply, progress) {
      const id = loop++;
      const chunk = chunkOf(n, size);
      const chunks = Math.ceil(n / chunk);
      const c: Loop = current = { loop: id, n, chunk, next: 0, results: new Array(n), done: new Uint8Array(chunks), left: chunks, items: 0, held: new Map(), compute, wake: null };
      if (o.minFree !== undefined && availableMemory() < o.minFree) {
        const last = [...members].reverse().find((m) => m.alive);
        if (last) { console.warn(`[eval] memory is low: process ${last.rank} stops`); stop(last); }
      }
      yieldToGame();
      // What members asked of this loop before rank 0 got here.
      const asked = early.get(id);
      early.delete(id);
      for (const m of members) if (m.alive && asked?.has(m.rank)) handOut(c, m, asked.get(m.rank)!);
      let lastProgress = 0;
      while (c.next < n) {
        const start = c.next;
        c.next += chunk;
        const out: unknown[] = [];
        for (let j = start; j < Math.min(n, start + chunk); j++) out.push(compute(j));
        take(c, start, out);
        const now = Date.now();
        if (progress && now - lastProgress > 250) { lastProgress = now; progress(c.items, n); }
        // After each chunk: the members' messages (results in, the next chunk out) go through.
        yieldToGame();
        await yieldNow();
      }
      while (c.left) {
        // Chunks held by a member that stopped are worked out here.
        for (const [k, rank] of [...c.held]) {
          if (members.some((m) => m.alive && m.rank === rank)) continue;
          const start = k * chunk, out: unknown[] = [];
          for (let j = start; j < Math.min(n, start + chunk); j++) out.push(compute(j));
          take(c, start, out);
        }
        if (!c.left) break;
        await new Promise<void>((r) => { c.wake = r; setTimeout(r, 200); });
        c.wake = null;
        yieldToGame();
        progress?.(c.items, n);
      }
      current = null;
      finishedLoops = id;
      for (const m of members) tell(m, { type: 'done', loop: id });
      for (let j = 0; j < n; j++) apply(j, c.results[j] as never);
    },
    async share(value) {
      const v = value();
      const id = shared++;
      for (const m of members) tell(m, { type: 'share', id, value: v });
      return v;
    },
    async close() {
      for (const m of members) stop(m);
    },
  };
}

/**
 * A member process's side (rank ≥ 1): it asks rank 0 for chunks (`send`, and rank 0's messages through `onMessage`),
 * works them out, sends their results, and goes on once rank 0 says the loop is over.
 */
export function joinProcessTeam(rank: number, size: number, send: (msg: MemberMessage) => void, onMessage: (fn: (msg: LeaderMessage) => void) => void): Team {
  const arrived = new Map<string, unknown[]>();
  const waiting = new Map<string, (v: unknown) => void>();
  const deliver = (key: string, value: unknown) => {
    const w = waiting.get(key);
    if (w) { waiting.delete(key); w(value); return; }
    let list = arrived.get(key);
    if (!list) { list = []; arrived.set(key, list); }
    list.push(value);
  };
  onMessage((msg) => {
    if (msg.type === 'take') deliver(`take:${msg.loop}`, msg.start);
    else if (msg.type === 'done') deliver(`done:${msg.loop}`, null);
    else deliver(`share:${msg.id}`, msg.value);
  });
  const next = (key: string) => {
    const list = arrived.get(key);
    if (list?.length) { const v = list.shift(); if (!list.length) arrived.delete(key); return Promise.resolve(v); }
    return new Promise<unknown>((r) => waiting.set(key, r));
  };
  let loop = 0, shared = 0, peak = 0, heapPeak = 0;
  const measure = () => {
    const h = v8.getHeapStatistics();
    peak = Math.max(peak, h.total_physical_size + h.external_memory);
    heapPeak = Math.max(heapPeak, h.total_heap_size);
  };
  return {
    rank, size,
    working: () => size,
    peak: () => peak,
    heapPeak: () => heapPeak,
    async forEach(n, compute) {
      const id = loop++;
      // Two in hand: the next one arrives while this one is worked out.
      send({ type: 'want', loop: id, count: 2 });
      for (;;) {
        const start = await next(`take:${id}`) as number;
        if (start < 0) break;
        const chunk = chunkOf(n, size);
        const results: unknown[] = [];
        for (let j = start; j < Math.min(n, start + chunk); j++) results.push(compute(j));
        send({ type: 'chunk', loop: id, start, results });
        measure();
      }
      // Any other 'none' for this loop, then the loop's end.
      arrived.delete(`take:${id}`);
      await next(`done:${id}`);
      measure();
      send({ type: 'peak', bytes: peak, heap: heapPeak });
    },
    async share<T>() { return await next(`share:${shared++}`) as T; },
    async close() {},
  };
}
