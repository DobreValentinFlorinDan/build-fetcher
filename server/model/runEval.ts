// npm run eval:builds and the app's "Evaluate builds" (in the job thread, server/jobWorker.ts) share this: evaluate the
// stored games, save the report next to the settings (never in the data folder) and the gate result the app reads.
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import v8 from 'node:v8';
import { AUTO_EVAL, evalMark } from '../autoEval.ts';
import { config } from '../config.ts';
import type { Db } from '../db.ts';
import { loadPatchHistory, type StaticData } from '../ddragon.ts';
import { buildExclusivityIndex } from '../engine/exclusivity.ts';
import { startGpu } from '../gpu/accelerate.ts';
import { regionPatches } from '../patches.ts';
import { heapMeasure, threadMeasure } from '../progress.ts';
import { startProcess, startWorker } from '../workers.ts';
import type { ProcessBoot } from './evalProcess.ts';
import { evaluate, EVAL_DEFAULTS, evalSteps, formatEval, type EvalBaseData, type EvalProgress, type EvalReport, type EvalSnapshot, type StepTime } from './evaluate.ts';
import { formatTimings } from './evalCompare.ts';
import { availableMemory, cores, leadProcessTeam, leadTeam, memoryReserve, OTHER_THREADS_HEAP, otherThreadsHeap, planThreads, shareValue, sharedHeapCeiling, solo, type Team } from './evalTeam.ts';
import { prepareGames } from './dataset.ts';
import { PARALLEL_FROM_GAMES, prepareOnThreads, prepareThreads } from './prepareTeam.ts';
import { ChampionProfiles } from './profiles.ts';
import {
  evalKey, evalOptions, evalReads, portableKey, keepVerdicts, planEvaluation, readBase, readBaseHeader, regionsNow, saveBase, UPDATE_RULES, updateFrom, verdictShift,
  type BaseHeader, type EvalPath,
} from './evalBase.ts';
import { evalDir, type EvalSummary } from './service.ts';
import { HISTORY_PATCHES } from './weights.ts';

/** What evaluateDataFolder takes: see there. */
export interface EvalFolderOptions {
  quick?: boolean; auto?: boolean; log?: (line: string) => void; progress?: (p: EvalProgress) => void;
  /** A full garbage collection, before each fold's models are built (evaluate's `collect`). */
  collect?: () => void;
  /**
   * Threads (0.11.0): a number, or by default as many as the cores and available memory allow (planThreads) for
   * `bytesPerGame` per thread, of which `heapPerGame` (0.19.0) counts against a heap the threads share. `control`: the
   * app's busy flag (server/jobs.ts).
   */
  threads?: number; bytesPerGame?: number; heapPerGame?: number; control?: SharedArrayBuffer | null; onThreads?: (threads: number) => void;
  /** Use GPU (0.13.0, server/gpu/): the draft similarities on the graphics card, checked first; none in champ select or a game. */
  gpu?: boolean;
  /** false: the report is returned, not saved (Slim stored data's checks, 0.14.0: your last evaluation stays the last). */
  save?: boolean;
  /**
   * Quick or full (0.16.0, docs/EVAL-UPDATE.md): 'auto' follows planEvaluation (model/evalBase.ts) and goes on to a full
   * evaluation when the quick update would change a verdict; 'update' adds the new games to the base whatever the plan
   * says (when there is one) and keeps what it finds; 'full' (the default) evaluates every game again.
   */
  path?: 'auto' | 'full' | 'update';
  /**
   * Grouped drafts (0.20.0, model/groups.ts; docs/EVAL-SCALING.md): a trial with the drafts in this many groups (0: about
   * 40 games each). Always a full evaluation, saved as eval-grouped-…; it never becomes the last evaluation the app
   * reads (latest.json) nor a base for quick updates, since its numbers are close to an exact evaluation's, not the same.
   */
  grouping?: number | null;
}

/**
 * `auto`: started by the app itself (server/autoEval.ts). Its report is saved as eval-auto-…, and only the newest
 * AUTO_EVAL.keepReports of those are kept; reports of evaluations you started are never deleted. A quick update's is
 * eval-[auto-]update-…
 */
export async function evaluateDataFolder(db: Db, staticData: StaticData, o: EvalFolderOptions = {}) {
  if (o.grouping !== undefined && o.grouping !== null) {
    return save(db, await measure(db, staticData, o, null), o, 'full', `grouped drafts trial (${o.grouping > 0 ? `${o.grouping} groups` : 'groups of about 40 games'})`, null);
  }
  const want = o.quick ? 'full' : o.path ?? 'full';
  let reason = o.quick ? 'quick check (--quick)' : 'full evaluation, as asked';
  if (want !== 'full') {
    const plan = planEvaluation(db, staticData);
    const base = want === 'update' || plan.path !== 'full' ? readBase() : null;
    if (base) {
      const run = await measure(db, staticData, o, base);
      const shift = verdictShift(base.data.report, run.report);
      const stale = (run.report.update?.missing ?? 0) + (run.report.update?.changed ?? 0);
      const gone = stale > UPDATE_RULES.maxMissing * base.data.known.length;
      if (want === 'update' || (!shift.decisive && !gone)) {
        const why = plan.path === 'update' || plan.path === 'none' ? plan.reason : `a quick update, as asked (${plan.reason})`;
        // A verdict the new games only move onto the line keeps the full evaluation's (verdictShift).
        if (shift.close.length) run.report = keepVerdicts(base.data.report, run.report, shift.close);
        run.report.update = { ...run.report.update!, fullAt: base.header.fullAt, reason: why, ...(shift.close.length ? { close: shift.close } : {}) };
        return save(db, run, o, 'update', why, base.header);
      }
      reason = gone
        ? `${stale.toLocaleString('en-US')} of the ${base.data.known.length.toLocaleString('en-US')} games the last full evaluation measured are no longer held out or changed since`
        : `the new games move ${shift.decisive}'s verdict clearly, so a full evaluation decides it`;
      o.log?.(`quick update not kept: ${reason}`);
    } else reason = plan.path === 'full' ? plan.reason : 'no base to add the new games to';
  }
  return save(db, await measure(db, staticData, o, null), o, 'full', reason, null);
}

/** One evaluation, measured and not yet saved: its report, the next base, and what it read. */
async function measure(db: Db, staticData: StaticData, o: EvalFolderOptions, base: { header: BaseHeader; data: EvalBaseData } | null) {
  // What it sees as it starts: games stored meanwhile count towards the next automatic evaluation.
  const mark = evalMark(db);
  const dataVersion = db.dataVersion();
  const memory = o.quick ? null : EVAL_DEFAULTS.memory;
  // What changed since each older patch (Data Dragon, cached on disk), as the model thread reads it.
  const data = memory && !staticData.history
    ? { ...staticData, history: await loadPatchHistory(staticData.versions ?? [staticData.version], HISTORY_PATCHES) }
    : staticData;
  const { scope, patches, read, sources } = evalReads(db, data, o.quick);
  const exact = evalOptions(scope, o.quick);
  const plain = o.grouping !== undefined && o.grouping !== null ? { ...exact, grouping: { groups: o.grouping } } : exact;
  const opts = base ? { ...plain, update: updateFrom(base.data, base.header, plain.maxGridGames) } : plain;
  // Reading the games is the first step; the evaluation's own follow.
  const steps = evalSteps(opts) + 1;
  o.progress?.({ step: 1, steps, label: base ? 'Reading the stored games (quick update)' : 'Reading the stored games' });
  const regions = regionPatches(db.gameVersions(), data);
  const profileRows = { current: db.championTotals(patches), older: db.olderChampionTotals(patches) };
  let team: Team | null = null, games = 0;
  const flags = o.control ? new Int32Array(o.control) : null;
  const busy = () => !!flags && Atomics.load(flags, 0) === 1;
  const total = db.modelGameCount(read, sources);
  const profiles = new ChampionProfiles(profileRows.current, profileRows.older, data);
  // The games are read and replayed on the model build's helper threads (0.15.1; at 54,000 games 43 s on this thread
  // alone, the other cores idle), in the order read: the same games. Champ select or a game: the rest here.
  const helpers = o.threads === 1 ? 1 : prepareThreads();
  const parallel = helpers > 1 && total >= PARALLEL_FROM_GAMES && !busy();
  const startingItemWindowMs = config.startingItemWindowMs;
  const gpu = o.gpu ? await startGpu({ thread: 'thread 1', busy: flags ? busy : undefined }) : null;
  let next: EvalBaseData | null = null;
  let timings: StepTime[] = [];
  const started = Date.now();
  const report: EvalReport = await evaluate({
    onTimings: (t) => {
      timings = t;
      const total = t.reduce((s, x) => s + x.ms, 0);
      o.log?.(`time: ${(total / 1000).toFixed(1)} s in all; ${[...t].sort((a, b) => b.ms - a.ms).slice(0, 3).map((x) => `${x.label} ${(x.ms / 1000).toFixed(1)} s`).join(', ')}`);
    },
    // One stored game at a time, each raw timeline dropped once its game is prepared; the live replay reads its sample again.
    rows: parallel ? db.modelGamesRawIter(read, sources) : db.modelGamesIter(read, sources), total, timelinesOf: (ids) => db.timelinesOf(ids),
    participants: db.modelParticipants(read, sources), classicPatches: patches, data,
    excl: buildExclusivityIndex(data),
    profiles,
    prepare: parallel ? (rows, participants, frames) => prepareOnThreads(rows, participants, {
      dataDir: config.dataDir, versions: data.versions ?? [data.version], profileRows, startingItemWindowMs, dicts: db.timelineDicts(),
      threads: helpers, frames, stop: busy, local: (rest, parts) => prepareGames(rest, parts, data, profiles, { startingItemWindowMs, frames }),
    }) : undefined,
    regionPatch: (p) => regions.get(p), recLog: db.recentRecommendations(10_000), startingItemWindowMs,
    log: o.log, progress: o.progress && ((p) => o.progress!({ ...p, step: p.step + 1, steps: p.steps + 1 })), collect: o.collect,
    team: o.threads === 1 ? undefined : {
      profileRows, regions,
      start: async (snapshot) => {
        games = snapshot.games.length + snapshot.olderGames.length;
        team = await startTeam(snapshot, {
          threads: o.threads, perThread: (o.bytesPerGame ?? THREAD_BYTES_PER_GAME) * games,
          perThreadHeap: (o.heapPerGame ?? THREAD_HEAP_PER_GAME) * games, control: o.control ?? null, log: o.log, gpu: gpu?.stats,
        });
        o.onThreads?.(team.size);
        return team;
      },
    },
    // The next base (0.16.0): a --quick check measures too little to be one, nor is a grouped trial.
    ...(o.quick || plain.grouping ? {} : { base: base?.data, salt: base?.data.salt ?? crypto.randomBytes(16).toString('hex'), onBase: (b: EvalBaseData) => { next = b; } }),
  }, opts).finally(() => gpu?.close());
  return {
    report, next: next as EvalBaseData | null, mark, dataVersion, read, key: evalKey(plain, sources, data), portable: portableKey(plain, sources, data),
    regions: regionsNow(db, data),
    current: mark.patch ? db.modelGameCount([mark.patch], sources) : 0, team: team as Team | null, games, timings, ms: Date.now() - started,
  };
}

/**
 * Saves a measured evaluation (unless `o.save` is false): its report, its base for the next quick update, and the
 * summary the app reads (latest.json), in that order, so a base is only read with the summary it belongs to.
 */
function save(db: Db, run: Awaited<ReturnType<typeof measure>>, o: EvalFolderOptions, ran: EvalPath, reason: string, header: BaseHeader | null) {
  const { report, mark, team: used, games, timings, ms } = run;
  const text = `${ran === 'update' ? `Quick update of the full evaluation of ${new Date(header!.fullAt).toISOString().slice(0, 16)}: ${reason}.\n\n` : ''}${formatEval(report)}`
    + (timings.length ? `\n\n${formatTimings(timings)}` : '');
  const trial = !!report.grouping;
  if (o.save === false) return { report, text, file: '', threads: used?.size ?? 1, bytesPerGame: null, heapPerGame: null, layout: THREAD_LAYOUT, path: ran, reason, timings, ms };
  const dir = evalDir();
  fs.mkdirSync(dir, { recursive: true });
  const stamp = new Date(report.evaluatedAt).toISOString().slice(0, 16).replace(/[:T]/g, '-');
  const name = `eval-${trial ? 'grouped-' : ''}${o.auto ? 'auto-' : ''}${ran === 'update' ? 'update-' : ''}${stamp}`;
  const file = path.join(dir, `${name}.txt`);
  fs.writeFileSync(path.join(dir, `${name}.json`), `${JSON.stringify({ ...report, timings }, null, 2)}\n`);
  fs.writeFileSync(file, `${text}\n`);
  // A grouped trial is kept beside the reports and nothing else: the app's verdicts and the next quick update stay the
  // last exact evaluation's.
  if (trial) return { report, text, file, threads: used?.size ?? 1, bytesPerGame: null, heapPerGame: null, layout: THREAD_LAYOUT, path: ran, reason, timings, ms };
  const full = ran === 'update' ? { evaluatedAt: header!.fullAt, mark: header!.fullMark } : { evaluatedAt: report.evaluatedAt, mark };
  // Another evaluation saved meanwhile (the command line beside the app, say): this quick update's base is out of date.
  if (ran === 'update' && readBaseHeader()?.lastAt !== header!.lastAt) {
    return { report, text: `${text}
(Not kept: another evaluation finished meanwhile.)`, file, threads: used?.size ?? 1, bytesPerGame: null, heapPerGame: null, layout: THREAD_LAYOUT, path: ran, reason, timings, ms };
  }
  if (run.next) {
    saveBase(run.next, {
      fullAt: full.evaluatedAt, fullMark: full.mark,
      fullHeldOut: ran === 'update' ? header!.fullHeldOut : run.next.known.length,
      fullPatch: ran === 'update' ? header!.fullPatch : mark.patch, fullCurrent: ran === 'update' ? header!.fullCurrent : run.current,
      lastAt: report.evaluatedAt, lastMark: mark, patches: run.read, regions: run.regions, key: run.key, portable: run.portable,
    });
  }
  const summary: EvalSummary = {
    evaluatedAt: report.evaluatedAt, gate: report.gate, bandwidth: report.chosen.h, k: report.chosen.k,
    lambda: report.state?.lambda ?? 1, dataVersion: run.dataVersion,
    ...(report.memory ? { memory: { gate: report.memory.gate, decay: report.memory.decay } } : {}),
    ...(report.scoring ? { scoring: { gate: report.scoring.gate, penalty: report.scoring.expected?.penalty ?? null } } : {}),
    ...(report.tuning ? { tuning: { gate: report.tuning.gate, choice: report.tuning.choice } } : {}),
    ...(report.swaps ? { swaps: { gate: report.swaps.gate } } : {}),
    mark, path: ran, full,
  };
  fs.writeFileSync(path.join(dir, 'latest.json'), `${JSON.stringify(summary, null, 2)}\n`);
  if (o.auto) pruneAutoReports(dir);
  // What the threads took: the next evaluation plans its threads from it. A thread measures its heap and buffers at
  // their peak; the process grows by about that much per thread (0.12.0, 54,000 games: 402 MB per thread against 427
  // measured; 0.11.0's heavier threads, a fifth more), so the margin below covers both.
  const threads = used?.size ?? 1;
  const bytesPerGame = used && used.peak() > 0 && games > 0 ? Math.round((used.peak() * THREAD_OVERHEAD) / games) : null;
  const heapPerGame = used && used.heapPeak() > 0 && games > 0 ? Math.round((used.heapPeak() * THREAD_OVERHEAD) / games) : null;
  return { report, text, file, threads, bytesPerGame, heapPerGame, layout: THREAD_LAYOUT, path: ran, reason, timings, ms };
}

/**
 * How the threads hold their data, for their measure (progress.ts threadMeasure): 2 since 0.12.0, where they keep no
 * results, share the purchases' chances and hold one fold's models at a time (half of 0.11.0's); 3 since 0.14.0, whose
 * draft models hold their contexts in flat arrays (a little less per game: a measure of 2 is not reused); 4 since
 * 0.19.0, whose threads let go of the stored rows and of each fold's validated rows (about half of 3's: a measure of
 * 3, 12,400 bytes a game on 112,750 games, would hold the next plan back to a thread or two).
 */
export const THREAD_LAYOUT = 4;

/** A thread's measured memory times this: what the process takes for it (see above), with a margin. */
const THREAD_OVERHEAD = 1.15;

/**
 * Memory one more thread takes per game it reads, before any evaluation has measured it on this computer: what one took
 * on the 54,000-game test database (0.12.0: 427 MB at its peak, 9,100 bytes per game read with the margin above; 0.14.0:
 * 9,413 against 9,758 the same day before it), and some more, an estimate. 0.19.0: a thread on 112,750 real games held
 * 631 MB at its peak in the settings grid (5,600 bytes a game, after a collection); its later steps hold more (patch
 * memory reads the older patches' games too). Per game read (about 150,000 there with the older patches'), with its buffers
 * and a margin: 6,500 until this computer has measured it.
 */
export const THREAD_BYTES_PER_GAME = 6_500;

/**
 * ...of which on the heap (0.19.0), what counts where the threads share one (the desktop app's 4 GB): the buffers (the
 * draft model's contexts, the lane indexes) live outside it. On 112,750 real games, 631 MB of heap and 158 MB of buffers.
 */
export const THREAD_HEAP_PER_GAME = 5_000;

/** Memory one more thread would take for an evaluation of `games` games: this computer's last measure, else the default. */
export const threadBytes = (games: number) => (threadMeasure(THREAD_LAYOUT) ?? THREAD_BYTES_PER_GAME) * games;
/** ...of which heap (0.19.0). */
export const threadHeap = (games: number) => (heapMeasure(THREAD_LAYOUT) ?? THREAD_HEAP_PER_GAME) * games;

/**
 * The threads an evaluation of `games` games would run on if it started now (model/evalTeam.ts). Where the threads
 * share one heap, the job thread is taken to hold half again what another thread does.
 */
export function plannedThreads(games: number) {
  const each = threadBytes(games), heap = threadHeap(games), ceiling = sharedHeapCeiling();
  // Member processes (0.22.0) each have a heap of their own: only the memory and the cores count.
  if (evalInProcesses()) return planThreads(each + PROCESS_OVERHEAD);
  return planThreads(each, { heap: ceiling ? { ceiling, used: OTHER_THREADS_HEAP + 1.5 * heap, each: heap } : undefined });
}

/**
 * Whether an evaluation's other members are processes (0.22.0, model/evalTeam.ts leadProcessTeam): in the desktop app,
 * where threads share one 4 GB heap, unless IRELIA_EVAL_PROCESSES=0; anywhere with IRELIA_EVAL_PROCESSES=1.
 */
export const evalInProcesses = () => process.env.IRELIA_EVAL_PROCESSES === '1' || (sharedHeapCeiling() !== null && process.env.IRELIA_EVAL_PROCESSES !== '0');

/** What a member process takes beyond its share of the evaluation: the runtime itself (an estimate, with a margin). */
export const PROCESS_OVERHEAD = 150 * 1024 ** 2;

/** A process's heap cap: half again what it should take, within the 4 GB a process's heap can reach. */
const processHeapMb = (perThread: number) => Math.max(512, Math.min(3_900, Math.ceil((perThread * 1.5) / 1024 ** 2)));

/**
 * The team for an evaluation (model/evalTeam.ts): `threads`, else as many as the cores and the memory available now
 * allow for `perThread` bytes each, every one with a copy of the snapshot (shareValue). One thread: the job thread alone.
 */
export async function startTeam(snapshot: EvalSnapshot, o: {
  threads?: number; perThread: number; control: SharedArrayBuffer | null; log?: (line: string) => void;
  /** Of `perThread`, the heap (0.19.0; the rest are buffers, outside a shared heap). Default: perThread. */
  perThreadHeap?: number;
  /** Use GPU: the job thread's session stats (server/gpu/accelerate.ts); the other threads use the GPU too and count there. */
  gpu?: SharedArrayBuffer;
}): Promise<Team> {
  // Where the threads share one heap (the desktop app), the others get what is left of it beyond the rest of the
  // process (0.15.1: as measured while it runs, otherThreadsHeap) and this thread: what it holds now (evaluate collects
  // its garbage first) and a fold's models to come.
  if (evalInProcesses()) {
    const team = await startProcessTeam(snapshot, o);
    if (team) return team;
  }
  const ceiling = sharedHeapCeiling();
  const others = otherThreadsHeap(o.control);
  const each = o.perThreadHeap ?? o.perThread;
  const heap = ceiling ? { ceiling, used: others + v8.getHeapStatistics().used_heap_size + each, each } : undefined;
  const size = o.threads ?? planThreads(o.perThread, { heap });
  const gb = (bytes: number) => (bytes / 1024 ** 3).toFixed(1);
  o.log?.(`threads: ${size} (${cores()} cores, ${gb(availableMemory())} GB available, about ${(o.perThread / 1024 ** 3).toFixed(2)} GB each, ${(each / 1024 ** 3).toFixed(2)} GB of it heap${heap ? `, ${gb(heap.ceiling - heap.used)} GB of shared heap left, ${gb(others)} GB kept for the app's other threads` : ''})`);
  if (size <= 1) return solo;
  // The purchases' chances are in memory every thread reads: handed over as they are, not copied with the rest.
  const { itemP, ...rest } = snapshot;
  const blob = shareValue(rest);
  const shared = itemP.buffer as SharedArrayBuffer;
  // Each thread's heap is capped at half again what it should take: one that outgrows it stops, and its games go to
  // this thread, instead of the shared heap running out under all of them.
  const heapMb = Math.max(256, Math.ceil((o.perThread * 1.5) / 1024 ** 2));
  const gpu = o.gpu ? { gpu: o.gpu, control: o.control } : {};
  return leadTeam(size, (rank, port, memory) => startWorker('eval', { blob, itemP: shared, rank, size, port, memory, ...gpu }, [port], heapMb), o.control,
    { minFree: memoryReserve() / 2 });
}

/**
 * The team with member processes (0.22.0): as many as the cores and the memory available now allow (each with a heap of
 * its own, so the app's shared one no longer limits them), each reading the snapshot from a temporary file. null: the
 * processes couldn't start (the threads are used instead), or one alone is planned (the job thread, `solo`, is returned).
 */
async function startProcessTeam(snapshot: EvalSnapshot, o: Parameters<typeof startTeam>[1]): Promise<Team | null> {
  const per = o.perThread + PROCESS_OVERHEAD;
  const size = o.threads ?? planThreads(per);
  const gb = (bytes: number) => (bytes / 1024 ** 3).toFixed(1);
  o.log?.(`processes: ${size} (${cores()} cores, ${gb(availableMemory())} GB available, about ${(per / 1024 ** 3).toFixed(2)} GB each, `
    + `each with a heap of its own${o.gpu ? '; Use GPU in this thread only' : ''})`);
  if (size <= 1) return solo;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-eval-'));
  const cleanUp = () => { try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* gone */ } };
  try {
    const { itemP, ...rest } = snapshot;
    const snapFile = path.join(dir, 'snapshot.bin'), itemFile = path.join(dir, 'item-p.bin');
    fs.writeFileSync(snapFile, v8.serialize(rest));
    fs.writeFileSync(itemFile, Buffer.from(itemP.buffer, itemP.byteOffset, itemP.byteLength));
    const heapMb = processHeapMb(o.perThread);
    const team = await leadProcessTeam(size, (rank) => {
      const proc = startProcess('eval', heapMb);
      proc.send({ type: 'boot', snapshot: snapFile, itemP: itemFile, rank, size } satisfies ProcessBoot);
      return proc;
    }, o.control, { minFree: memoryReserve() / 2 });
    // The copies are read: the files go now (each process said ready, or was stopped).
    cleanUp();
    if (team.working() <= 1) {
      o.log?.('processes: none started; threads instead');
      await team.close();
      return null;
    }
    return team;
  } catch (err) {
    cleanUp();
    o.log?.(`processes: couldn't start (${err instanceof Error ? err.message : String(err)}); threads instead`);
    return null;
  }
}

export { evalReads } from './evalBase.ts';

/** How many stored games an evaluation started now would read: what its time grows with (server/progress.ts). */
export function evalSize(db: Db, data: StaticData): number {
  const { read, sources } = evalReads(db, data);
  return db.modelGameCount(read, sources);
}

/** Keeps the newest AUTO_EVAL.keepReports automatic reports (their names sort by time). */
export function pruneAutoReports(dir: string, keep = AUTO_EVAL.keepReports) {
  // By time alone: a quick update's (eval-auto-update-<stamp>) sorts among the full ones, not after them all (0.16.0).
  const names = [...new Set(fs.readdirSync(dir).flatMap((f) => {
    const m = /^(eval-auto-(?:update-)?([\d-]+))\.(?:txt|json)$/.exec(f);
    return m ? [`${m[2]}|${m[1]}`] : [];
  }))].sort().map((x) => x.slice(x.indexOf('|') + 1));
  for (const name of names.slice(0, Math.max(0, names.length - keep))) {
    for (const ext of ['txt', 'json']) fs.rmSync(path.join(dir, `${name}.${ext}`), { force: true });
  }
}
