// A model build's stored games prepared on several threads (0.15.0): replaying each game's timeline (model/dataset.ts
// prepareGames) is about 95 % of a build, and one game doesn't depend on another. This thread reads the games in their
// order and sends them out 250 at a time; the helpers (model/prepareWorker.ts) prepare them; they are put back in the
// order they were read, so the build is the same as on one thread. While it waits, the model thread answers requests
// with the model it has. The evaluation reads its games this way too (0.15.1, model/runEval.ts).
import type { Worker } from 'node:worker_threads';
import { unpack } from '../codec.ts';
import { startWorker } from '../workers.ts';
import type { ModelGameRow, ParticipantLite, PreparedGame } from './dataset.ts';
import type { ChampionTotals } from './profiles.ts';
import { availableMemory, cores } from './evalTeam.ts';

/** Games per message. */
const CHUNK = 250;
/** A helper's heap at most: Data Dragon's files and 250 games' timelines. */
const HELPER_HEAP_MB = 512;
/** Below this many games one thread is about as fast (starting a helper takes a moment). */
export const PARALLEL_FROM_GAMES = 4000;

/** Helpers for a build: every core but this one, as the free memory allows, at most 6. */
export function prepareThreads() {
  return Math.max(1, Math.min(6, cores() - 1, Math.floor(availableMemory() / 3 / (HELPER_HEAP_MB * 1024 * 1024))));
}

export class BuildCancelled extends Error {
  constructor() { super('The build stopped (champ select, a game or the queue began)'); }
}

/** A stored game as read for the helpers (Db.modelGamesRaw): its timeline as stored, compressed or not. */
export type RawGameRow = ModelGameRow & { timeline_z?: Uint8Array | null };

/** Each row's timeline as db.ts timeline_json reads it, in place (a helper's batch, or the rest on this thread). */
export function decodeTimelines(rows: RawGameRow[], dict: (id: number) => Buffer | undefined) {
  for (const r of rows) {
    if (r.timeline_z instanceof Uint8Array) r.timeline = unpack(r.timeline_z, dict).toString('utf8');
    r.timeline_z = null;
  }
}

export interface PrepareTeamOptions {
  dataDir: string;
  /** Data Dragon's versions as this thread loaded them (the helpers load the same files). */
  versions: string[];
  profileRows: { current: ChampionTotals[]; older: ChampionTotals[] };
  startingItemWindowMs: number;
  /** The timelines' dictionaries (Db.timelineDicts): `rows` carry each timeline as stored, decoded by the helpers. */
  dicts: [number, Uint8Array][];
  threads: number;
  /** Where each game's state frames go in this thread (dataset.ts frameSlabs). */
  frames?: (length: number) => Float32Array;
  /** Checked before each batch: true stops the build (BuildCancelled). */
  stop?: () => boolean;
  /**
   * With this, `stop` doesn't cancel (0.15.1, the evaluation's reading): the batches out are waited for, and `local`
   * prepares the rest here, in the same order (prepareGames: each timeline decoded as it comes, dropped once used).
   */
  local?: (rows: Iterable<ModelGameRow>, participants: ParticipantLite[]) => PreparedGame[];
}

/**
 * The games of `rows`, prepared on `o.threads` helpers, in the order read. Each row's timeline is dropped once sent
 * (the rows may be kept: the evaluation keeps them for the lane builds).
 */
export async function prepareOnThreads(rows: Iterable<RawGameRow>, participants: ParticipantLite[], o: PrepareTeamOptions): Promise<PreparedGame[]> {
  const byMatch = new Map<string, ParticipantLite[]>();
  for (const p of participants) {
    let list = byMatch.get(p.match_id);
    if (!list) { list = []; byMatch.set(p.match_id, list); }
    list.push(p);
  }
  const workers: Worker[] = [];
  const end = () => { for (const w of workers) void w.terminate(); };
  try {
    for (let i = 0; i < Math.max(1, o.threads); i++) {
      workers.push(startWorker('prepare', { dataDir: o.dataDir, versions: o.versions, profileRows: o.profileRows, startingItemWindowMs: o.startingItemWindowMs, dicts: o.dicts }, [], HELPER_HEAP_MB));
    }
    // Each helper loads Data Dragon's files first.
    await Promise.all(workers.map((w) => new Promise<void>((resolve, reject) => {
      const onMessage = (m: { ready?: boolean }) => { if (m.ready) { w.off('error', reject); resolve(); } };
      w.once('message', onMessage);
      w.once('error', reject);
    })));
    return await new Promise<PreparedGame[]>((resolve, reject) => {
      const it = rows[Symbol.iterator]();
      const batches: PreparedGame[][] = [];
      let seq = 0, inFlight = 0, read = false, over = false, stopped = false;
      const fail = (err: Error) => { if (!over) { over = true; reject(err); } };
      /** The next CHUNK rows read and their players. */
      const take = () => {
        const batch: RawGameRow[] = [], parts: ParticipantLite[] = [];
        while (!read && batch.length < CHUNK) {
          const r = it.next();
          if (r.done) { read = true; break; }
          batch.push(r.value);
          const ps = byMatch.get(r.value.match_id);
          if (ps) parts.push(...ps);
        }
        return { batch, parts };
      };
      /** Stopped with `local`: once the batches out are in, the rest here, one game at a time. */
      const finishHere = () => {
        if (over || inFlight > 0) return;
        over = true;
        const dicts = new Map(o.dicts.map(([id, bytes]) => [id, Buffer.from(bytes)]));
        function* rest() {
          for (let r = it.next(); !r.done; r = it.next()) {
            decodeTimelines([r.value], (id) => dicts.get(id));
            yield r.value;
            r.value.timeline = null;
          }
        }
        try {
          batches[seq] = o.local!(rest(), participants);
          resolve(batches.flat());
        } catch (err) { reject(err as Error); }
      };
      const send = (w: Worker) => {
        if (over) return;
        if (!stopped && o.stop?.()) {
          if (!o.local) { fail(new BuildCancelled()); return; }
          stopped = true;
        }
        if (stopped) { finishHere(); return; }
        const { batch, parts } = take();
        if (!batch.length) {
          if (inFlight === 0 && !over) { over = true; resolve(batches.flat()); }
          return;
        }
        inFlight++;
        w.postMessage({ seq: seq++, rows: batch, participants: parts });
        // Copied over to the helper: the timelines (most of a row) aren't needed here any more.
        // Set to null, not deleted: a deleted property turns a kept row into a dictionary (0.19.0).
        for (const r of batch) { r.timeline = null; r.timeline_z = null; }
      };
      for (const w of workers) {
        w.on('message', (m: { seq?: number; prepared?: PreparedGame[]; error?: string }) => {
          if (over || m.seq === undefined) return;
          if (m.error || !m.prepared) { fail(new Error(`Preparing the games failed: ${m.error ?? 'no result'}`)); return; }
          // Frames into this thread's large blocks, as on one thread (they are dropped once the state model is fitted).
          if (o.frames) for (const g of m.prepared) if (g.frames) { const f = o.frames(g.frames.length); f.set(g.frames); g.frames = f; }
          batches[m.seq] = m.prepared;
          inFlight--;
          send(w);
        });
        w.on('error', (err) => fail(new Error(`A helper thread stopped: ${(err as Error).message}`)));
        w.on('exit', (code) => { if (!over) fail(new Error(`A build thread stopped (code ${code})`)); });
        send(w);
      }
    });
  } finally { end(); }
}
