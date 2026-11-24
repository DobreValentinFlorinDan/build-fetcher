// Evaluate builds on every core (0.11.0, model/evalTeam.ts): the threads it plans for the cores and the memory, and the
// same report to the last digit on any number of threads, with a thread that dies on the way, or while the app is busy.
import assert from 'node:assert/strict';
import type { ChildProcess } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { before, test } from 'node:test';
import v8 from 'node:v8';
import type { Worker } from 'node:worker_threads';
import type { StaticData } from '../ddragon.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { apAdGames, modelInputs, rng } from '../test/modelFixtures.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { startProcess, startWorker } from '../workers.ts';
import { EVAL_DEFAULTS, evaluate, type EvalOptions, type EvalReport, type EvalSnapshot } from './evaluate.ts';
import { chunkOf, controlFlags, leadProcessTeam, leadTeam, memoryReserve, OTHER_THREADS_HEAP, otherThreadsHeap, planThreads, setBusyFlag, setOtherHeap, shareValue, solo, type Team } from './evalTeam.ts';
import { prepareGames, type PreparedGame } from './dataset.ts';
import { ChampionProfiles } from './profiles.ts';
import { startTeam } from './runEval.ts';

let data: StaticData;
let excl: ExclusivityIndex;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); });

const GB = 1024 ** 3;

test('team: every core the available memory holds, a reserve left free, at least one', () => {
  const total = 16 * GB;
  // 16 GB: 2.4 GB stays free (15 %). 10 GB available leaves 7.6 GB, 3 threads of 2 GB beyond the first.
  assert.equal(memoryReserve(total), 0.15 * total);
  assert.equal(planThreads(2 * GB, { available: 10 * GB, total, cores: 16 }), 4);
  // The cores hold it back first.
  assert.equal(planThreads(0.5 * GB, { available: 10 * GB, total, cores: 8 }), 8);
  // Less available than the reserve: the job thread alone.
  assert.equal(planThreads(GB, { available: 2 * GB, total, cores: 8 }), 1);
  // Small computers keep at least 1.5 GB.
  assert.equal(memoryReserve(4 * GB), 1.5 * GB);
  assert.equal(planThreads(GB, { available: 3 * GB, total: 4 * GB, cores: 4 }), 2);
  // Threads sharing one heap (the desktop app): what is left of it beyond the rest of the process holds the others.
  assert.equal(planThreads(GB, { available: 12 * GB, total, cores: 16, heap: { ceiling: 4 * GB, used: 2.5 * GB } }), 2);
  assert.equal(planThreads(GB, { available: 12 * GB, total, cores: 16, heap: { ceiling: 4 * GB, used: 4.5 * GB } }), 1);
  // Only a thread's heap counts against the shared heap (0.19.0); its buffers count against the memory alone.
  assert.equal(planThreads(GB, { available: 12 * GB, total, cores: 16, heap: { ceiling: 4 * GB, used: 2.5 * GB, each: 0.5 * GB } }), 4);
  // Chunks: about 24 per thread, 1 to 32 games each.
  assert.equal(chunkOf(10, 4), 1);
  assert.equal(chunkOf(30_000, 4), 32);
  assert.equal(chunkOf(2000, 4), 21);
  assert.equal(chunkOf(4_000_000, 4), 62, 'no more chunks than the shared table holds');
});

test('team: the other threads\' heap as measured, a quarter and 256 MB more, at least 1 GB (0.15.1)', () => {
  const control = controlFlags();
  assert.equal(otherThreadsHeap(null), OTHER_THREADS_HEAP, 'no flags: as before');
  assert.equal(otherThreadsHeap(control), OTHER_THREADS_HEAP, 'not measured yet');
  setOtherHeap(control, 0.4 * GB);
  assert.equal(otherThreadsHeap(control), OTHER_THREADS_HEAP, 'little held: never below 1 GB');
  setOtherHeap(control, 2 * GB);
  assert.equal(otherThreadsHeap(control), 2.75 * GB);
  // The busy flag beside it is untouched.
  setBusyFlag(control, true);
  assert.equal(new Int32Array(control)[0], 1);
  assert.equal(otherThreadsHeap(control), 2.75 * GB);
});

test('team: one thread computes and applies each item in turn', async () => {
  const seen: string[] = [];
  await solo.forEach(3, (j) => j * 2, (j, r) => seen.push(`${j}:${r}`));
  assert.deepEqual(seen, ['0:0', '1:2', '2:4']);
});

const ENGINE = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};
// Every comparison the app's evaluation runs, on a small grid: the draft model, patch memory, runes against the draft, the replay.
const OPTS: EvalOptions = { ...EVAL_DEFAULTS, engine: ENGINE, lambdas: [0.3, 1], bandwidths: [0.7, Infinity], ks: [100], bootstrap: 100, live: true, maxLiveGames: 80, minGames: 100 };

function run(start?: (snapshot: EvalSnapshot) => Promise<Team>) {
  const recent = apAdGames(500, 40, 41, { effect: true, randomOpponent: true }).map((g, i) => ({ ...g, version: i % 2 ? '16.18.1.1' : '16.19.1.1' }));
  const past = apAdGames(400, 40, 42, { effect: true, randomOpponent: true }).map((g, i) => ({ ...g, id: `KR_${50_000 + i}`, version: `16.${17 - (i % 2)}.1.1` }));
  const inputs = modelInputs([...recent, ...past]);
  return evaluate({
    ...inputs, classicPatches: ['16.19', '16.18'], data, excl, profiles: new ChampionProfiles(inputs.totals, [], data),
    regionPatch: () => undefined, startingItemWindowMs: 60_000,
    ...(start ? { team: { profileRows: { current: inputs.totals, older: [] }, regions: new Map(), start } } : {}),
  }, OPTS);
}
const same = (a: EvalReport, b: EvalReport) => {
  const { evaluatedAt: _a, ...x } = a;
  const { evaluatedAt: _b, ...y } = b;
  assert.deepEqual(y, x);
};

let alone: EvalReport;
before(async () => { alone = await run(); });

test('team: three threads report what one does, to the last digit', async () => {
  let team: Team | null = null;
  const shared = await run(async (snapshot) => (team = await startTeam(snapshot, { threads: 3, perThread: 0, control: null })));
  assert.equal(team!.size, 3);
  assert.equal(team!.working(), 1, 'its threads stopped once it ended');
  assert.ok(team!.peak() > 0 && team!.heapPeak() > 0, 'what a thread took is measured, and its heap (the next plan goes by them)');
  assert.ok(alone.memory && alone.scoring && alone.stability, 'every comparison ran');
  same(shared, alone);
});

test('team of processes (0.22.0): three processes report what one thread does, to the last digit', async () => {
  const was = process.env.IRELIA_EVAL_PROCESSES;
  process.env.IRELIA_EVAL_PROCESSES = '1';
  try {
    let team: Team | null = null;
    const lines: string[] = [];
    const shared = await run(async (snapshot) => (team = await startTeam(snapshot, { threads: 3, perThread: 64 * 1024 ** 2, control: null, log: (l) => lines.push(l) })));
    assert.ok(lines.some((l) => /^processes: 3 /.test(l)), lines.join('\n'));
    assert.equal(team!.size, 3);
    assert.equal(team!.working(), 1, 'its processes ended with it');
    assert.ok(team!.peak() > 0 && team!.heapPeak() > 0, 'what a process took is measured');
    same(shared, alone);
  } finally {
    if (was === undefined) delete process.env.IRELIA_EVAL_PROCESSES; else process.env.IRELIA_EVAL_PROCESSES = was;
  }
});

test('team of processes: one that dies on the way leaves its chunks to the job thread, and the report is the same', async () => {
  const procs: ChildProcess[] = [];
  const chunksFrom: number[] = [];
  const shared = await run(async (snapshot) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-proc-test-'));
    const { itemP, ...rest } = snapshot;
    fs.writeFileSync(path.join(dir, 's.bin'), v8.serialize(rest));
    fs.writeFileSync(path.join(dir, 'p.bin'), Buffer.from(itemP.buffer, itemP.byteOffset, itemP.byteLength));
    const team = await leadProcessTeam(3, (rank) => {
      const p = startProcess('eval');
      p.send({ type: 'boot', snapshot: path.join(dir, 's.bin'), itemP: path.join(dir, 'p.bin'), rank, size: 3 });
      p.on('message', (msg: { type?: string }) => {
        if (msg?.type !== 'chunk') return;
        chunksFrom[rank] = (chunksFrom[rank] ?? 0) + 1;
        // Process 2 is killed right after its first chunk is in, with the next one in hand: rank 0 works it out.
        if (rank === 2 && chunksFrom[2] === 1) p.kill();
      });
      procs[rank] = p;
      return p;
    }, null);
    return { ...team, async close() { await team.close(); fs.rmSync(dir, { recursive: true, force: true }); } };
  });
  assert.equal(chunksFrom[2], 1, 'process 2 sent one chunk and was killed');
  assert.ok((chunksFrom[1] ?? 0) > 10, `process 1 worked out chunks (${chunksFrom[1]})`);
  same(shared, alone);
});

test('team: a thread that dies on the way leaves its games to the job thread, and the report is the same', async () => {
  const workers: Worker[] = [];
  const working: number[] = [];
  let computed = 0;
  const shared = await run(async (snapshot) => {
    const blob = shareValue(snapshot);
    const team = await leadTeam(3, (rank, port, memory) => (workers[rank] = startWorker('eval', { blob, rank, size: 3, port, memory }, [port])), null);
    return {
      ...team,
      async forEach(n, compute, apply, progress) {
        working.push(team.working());
        // Mid-loop, while it works out its games: what it took and hasn't sent is worked out again here.
        await team.forEach(n, (j) => { if (++computed === 200) void workers[2].terminate(); return compute(j); }, apply, progress);
      },
    };
  });
  assert.equal(working[0], 3);
  assert.equal(working.at(-1), 2, 'it died on the way');
  same(shared, alone);
});

test('team: champ select or a game stops the other threads (their memory goes back), and the report is the same', async () => {
  const control = controlFlags();
  const working: number[] = [];
  let computed = 0;
  const shared = await run(async (snapshot) => {
    const team = await startTeam(snapshot, { threads: 3, perThread: 0, control });
    return {
      ...team,
      async forEach(n, compute, apply, progress) {
        working.push(team.working());
        // Champ select begins mid-loop, and ends soon after: the job thread finishes alone.
        await team.forEach(n, (j) => {
          if (++computed === 150) { setBusyFlag(control, true); setTimeout(() => setBusyFlag(control, false), 2000); }
          return compute(j);
        }, apply, progress);
      },
    };
  });
  assert.equal(working[0], 3);
  assert.equal(working.at(-1), 1, 'the other threads stopped');
  same(shared, alone);
});

test('team: late-game swaps on three threads report what one does (0.17.0)', async () => {
  // Every game decides about its boots late: three threads must go through the swaps' interval together.
  const inject = (games: PreparedGame[]) => {
    const r = rng(9);
    for (const g of games) g.swaps = [{ held: 3047, item: r() < (g.win ? 0.7 : 0.3) ? 3065 : 0, t: 30 * 60_000, w: 1, x: null, live: [NaN, NaN, NaN, NaN, NaN, NaN, NaN, 30] }];
    return games;
  };
  const runSwaps = (start?: (snapshot: EvalSnapshot) => Promise<Team>) => {
    const inputs = modelInputs(apAdGames(500, 40, 41, { effect: true, randomOpponent: true }));
    const profiles = new ChampionProfiles(inputs.totals, [], data);
    return evaluate({
      ...inputs, data, excl, profiles, regionPatch: () => undefined, startingItemWindowMs: 60_000,
      prepare: async (rows, participants, frames) => inject(prepareGames(rows, participants, data, profiles, { startingItemWindowMs: 60_000, frames })),
      ...(start ? { team: { profileRows: { current: inputs.totals, older: [] }, regions: new Map(), start } } : {}),
    }, { ...OPTS, live: false, memory: null, scoring: false, tuning: null });
  };
  const one = await runSwaps();
  assert.ok(one.swaps && one.swaps.games >= OPTS.minGames, 'the swaps reached their interval');
  const three = await runSwaps(async (snapshot) => startTeam(snapshot, { threads: 3, perThread: 0, control: null }));
  assert.deepEqual(three.swaps, one.swaps);
});
