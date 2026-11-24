// Evaluate builds by itself (server/autoEval.ts): with Follow the evaluation on, what the plan says (0.16.0): a quick
// update after 300 new games, at most every hour; a full evaluation after a new patch or 300 games (3 % on a large
// database) since the last full one, at most every 6 hours after it; each after 10 minutes without champ select or a
// game; champ select or a game pauses it. The plan is worked out on or off, for the Data tab. The new-game count
// survives archiving and compaction; summaries from before 0.8.4 still give one. Real SQL on a real database.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import type { AutoEvalStatus, EvaluationPlan } from '../shared/types.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-auto-eval-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');

const { AUTO_EVAL, AutoEvaluation, evalMark, markOf, newGamesSince, newestPatch } = await import('./autoEval.ts');
const { pruneAutoReports } = await import('./model/runEval.ts');
const { CANCELLED } = await import('./jobs.ts');
const { captureTimeline, participantRows, CAPTURE_VERSION } = await import('./capture.ts');
const { openDb } = await import('./db.ts');
const { captureGame } = await import('./scraper/ingest.ts');
const { apAdGames, synthMatch } = await import('./test/modelFixtures.ts');
type EvalSummary = import('./model/service.ts').EvalSummary;

after(() => { fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

const H = 3600_000, MIN = 60_000;

/** A database stand-in: stored game rows (ids only grow) and the builds seen per patch. */
function fakeDb() {
  const d = {
    rows: [] as number[],
    versions: [{ patch: '16.19', first_seen: 0 }] as { patch: string; first_seen: number }[],
    reads: 0,
    add(n: number) { const top = d.rows.at(-1) ?? 0; for (let i = 1; i <= n; i++) d.rows.push(top + i); },
    gameMark: () => { d.reads++; return { row: d.rows.at(-1) ?? 0, games: d.rows.length }; },
    gamesAfterRow: (row: number) => d.rows.filter((r) => r > row).length,
    gameVersions: () => d.versions.map((v) => ({ platform: 'KR', game_version: `${v.patch}.1.1`, last_seen: v.first_seen, games: 1, ...v })),
  };
  return d;
}

function harness(o: { evaluatedAt?: number } = {}) {
  let now = 100 * H;
  const db = fakeDb();
  db.add(1000);
  const state = {
    following: true, quiet: true, busy: false,
    summary: { evaluatedAt: o.evaluatedAt ?? now - 7 * H, gate: 'failed', bandwidth: 1, k: 30, lambda: 1, dataVersion: '1000:0:0:0', mark: { row: 1000, games: 1000, patch: '16.19' } } as EvalSummary | null,
    running: null as { auto?: string } | null,
    // What planEvaluation says (model/evalBase.ts): full until a test says otherwise, as for a summary before 0.16.0.
    plan: { path: 'full', reason: 'no full evaluation saved by this version yet', newGames: 0, sinceFull: 0, fullAt: null } as EvaluationPlan,
    planError: null as string | null,
  };
  const started: string[] = [];
  const paths: string[] = [];
  const paused: number[] = [];
  let plans = 0;
  let settle: { resolve: () => void; reject: (e: Error) => void } | null = null;
  const statuses: AutoEvalStatus[] = [];
  const auto = new AutoEvaluation({
    following: () => state.following, quiet: () => state.quiet, busy: () => state.busy,
    summary: () => state.summary, db: () => db, running: () => state.running,
    plan: () => {
      plans++;
      if (state.planError) throw new Error(state.planError);
      return state.plan;
    },
    start: (why, planned) => {
      started.push(why);
      paths.push(planned);
      state.running = { auto: why };
      return new Promise<void>((resolve, reject) => { settle = { resolve, reject }; }).finally(() => { state.running = null; });
    },
    pause: async () => { paused.push(now); settle?.reject(new Error(CANCELLED)); },
    changed: (s) => statuses.push(s),
    now: () => now,
  });
  const tick = () => new Promise((r) => setImmediate(r));
  return {
    auto, db, state, started, paths, paused, statuses, tick,
    advance: (ms: number) => { now += ms; },
    get now() { return now; },
    /** How many times the plan was worked out. */
    get plans() { return plans; },
    finish: async (summary?: Partial<EvalSummary>) => {
      if (summary) state.summary = { ...state.summary!, ...summary };
      settle!.resolve(); settle = null; await tick();
    },
    fail: async (message: string) => { settle!.reject(new Error(message)); settle = null; await tick(); },
  };
}

test('automatic evaluation: off with Follow the evaluation off; on, it starts after 300 new games once 10 minutes passed without champ select or a game', async () => {
  const h = harness();
  h.state.following = false;
  h.db.add(500);
  h.advance(30 * MIN);
  h.auto.check();
  assert.deepEqual([h.auto.status().on, h.started.length, h.db.reads], [false, 0, 0], 'off: nothing read, nothing started');
  assert.deepEqual([h.plans, h.auto.plan()?.path, h.auto.status().path], [1, 'full', null], 'the plan is still worked out: the Data tab shows it');

  h.state.following = true;
  h.db.rows.splice(1000);   // back to the evaluation's 1,000 games
  h.db.add(299);
  h.auto.check();
  assert.deepEqual([h.auto.status().newGames, h.auto.status().waiting, h.started.length], [299, null, 0], 'not due yet');
  h.db.add(1);
  h.state.quiet = false;   // champ select
  h.auto.check();
  assert.deepEqual([h.auto.status().waiting, h.started.length], ['quiet', 0]);
  h.state.quiet = true;
  h.auto.check();
  assert.equal(h.auto.status().notBefore, h.now + AUTO_EVAL.quietMs, 'counts from the end of champ select');
  h.advance(9 * MIN);
  h.auto.check();
  assert.equal(h.started.length, 0);
  h.advance(MIN);
  h.auto.check();
  assert.deepEqual([h.started, h.paths], [['300 new games'], ['full']]);

  // It ends: its mark is the new baseline.
  await h.finish({ evaluatedAt: h.now, mark: { row: 1300, games: 1300, patch: '16.19' } });
  assert.deepEqual([h.auto.status().newGames, h.auto.status().waiting], [0, null]);
  assert.ok(h.statuses.length >= 3, 'status changes are pushed');
});

test('automatic evaluation: a new patch starts one, at most every 6 hours after the last evaluation', async () => {
  const h = harness({ evaluatedAt: 100 * H - H });
  h.advance(10 * MIN);
  h.db.versions.push({ patch: '16.20', first_seen: h.now });
  h.auto.check();
  assert.deepEqual([h.auto.status().newPatch, h.auto.status().waiting, h.auto.status().notBefore], ['16.20', 'spacing', 100 * H + 5 * H]);
  h.advance(5 * H - 10 * MIN - MIN);
  h.auto.check();
  assert.equal(h.started.length, 0);
  h.advance(MIN);
  h.auto.check();
  assert.deepEqual(h.started, ['new patch 16.20']);
});

test("automatic evaluation: champ select pauses it, which doesn't count as a try; a failure waits 6 hours; a click joining it is never paused", async () => {
  const h = harness();
  h.db.add(400);
  h.advance(10 * MIN);
  h.auto.check();
  assert.equal(h.started.length, 1);
  h.state.quiet = false;
  h.auto.check();
  await h.tick();
  assert.equal(h.paused.length, 1, 'paused at once');
  assert.equal(h.state.running, null);
  h.state.quiet = true;
  h.auto.check();
  assert.equal(h.auto.status().waiting, 'quiet');
  h.advance(10 * MIN);
  h.auto.check();
  assert.equal(h.started.length, 2, 'starts over after a quiet spell, no 6-hour wait');

  await h.fail('The evaluation stopped');
  const failedAt = h.now;
  h.advance(10 * MIN);
  h.auto.check();
  assert.deepEqual([h.auto.status().waiting, h.auto.status().notBefore, h.started.length], ['spacing', failedAt + 6 * H, 2]);

  // A running evaluation you started (or joined with a click): champ select doesn't stop it.
  h.state.running = {};
  h.state.quiet = false;
  h.auto.check();
  assert.equal(h.paused.length, 1);
});

test('automatic evaluation: while the data folder moves nothing is read or started', () => {
  const h = harness();
  h.db.add(400);
  h.advance(10 * MIN);
  h.state.busy = true;
  h.auto.check();
  assert.deepEqual([h.db.reads, h.started.length], [0, 0]);
  h.state.busy = false;
  h.auto.check();
  assert.equal(h.started.length, 1);
});

test('automatic evaluation: never evaluated, every stored game counts; a summary from before 0.8.4 still gives its games and patch', () => {
  const db = fakeDb();
  db.add(250);
  assert.equal(newGamesSince(db, markOf(null, [])), 250);
  db.versions = [{ patch: '16.18', first_seen: 1 }, { patch: '16.19', first_seen: 50 }, { patch: '16.20', first_seen: 200 }];
  const old = { evaluatedAt: 100, gate: 'failed', bandwidth: 1, k: 30, lambda: 1, dataVersion: '200:99:180:200' } as EvalSummary;
  assert.deepEqual(markOf(old, db.versions), { row: null, games: 200, patch: '16.19' });
  assert.equal(newGamesSince(db, markOf(old, db.versions)), 50);
  assert.equal(newestPatch(db.versions), '16.20');
});

test('automatic evaluation: new games are counted by row, so archiving older games hides none; after Compact database, by count', () => {
  const db = fakeDb();
  db.add(1100);
  db.rows.splice(0, 400);   // archived: 1..400
  assert.equal(newGamesSince(db, { row: 1000, games: 1000, patch: null }), 100);
  db.rows = Array.from({ length: 1100 }, (_, i) => i + 1);   // compacted: renumbered from 1, the mark's row was 5000
  assert.equal(newGamesSince(db, { row: 5000, games: 1000, patch: null }), 100);
});

test('automatic evaluation: the real database gives the mark, and games stored after it', () => {
  const db = openDb(path.join(root, 'irelia.db'));
  try {
    const store = (n: number, patch: string, from: number) => db.tx(() => {
      for (const s of apAdGames(n, 5, from).map((g, i) => ({ ...g, id: `KR_${from * 1000 + i}`, version: `${patch}.1.1` }))) {
        const { match, timeline } = synthMatch(s);
        const { row } = captureGame(match, timeline, match.info.participants[0], 'otp', undefined);
        db.insertMatch({ id: s.id, platform: 'KR', patch, version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
        db.insertGame(row);
        db.insertParticipants(s.id, participantRows(match));
        db.insertTimeline(s.id, captureTimeline(timeline).interval, JSON.stringify(captureTimeline(timeline)));
        db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
      }
    });
    store(20, '16.19', 1);
    const mark = evalMark(db);
    assert.deepEqual([mark.games, mark.patch], [20, '16.19']);
    store(6, '16.20', 2);
    db.raw.prepare("DELETE FROM matches WHERE match_id LIKE 'KR_1%'").run();   // older games go (archived, say)
    assert.equal(newGamesSince(db, mark), 6);
    assert.equal(newestPatch(db.gameVersions()), '16.20');
  } finally { db.close(); }
});

test('automatic evaluation: only the newest 20 automatic reports are kept, yours never go', () => {
  const dir = path.join(root, 'evals');
  fs.mkdirSync(dir, { recursive: true });
  const stamp = (i: number) => `2026-10-${String(1 + Math.floor(i / 4)).padStart(2, '0')}-${String(i % 4).padStart(2, '0')}-00`;
  for (let i = 0; i < 23; i++) for (const ext of ['txt', 'json']) fs.writeFileSync(path.join(dir, `eval-auto-${stamp(i)}.${ext}`), '');
  for (const ext of ['txt', 'json']) fs.writeFileSync(path.join(dir, `eval-2026-09-01-10-00.${ext}`), '');
  fs.writeFileSync(path.join(dir, 'latest.json'), '{}');
  pruneAutoReports(dir);
  const left = fs.readdirSync(dir);
  assert.equal(left.filter((f) => f.startsWith('eval-auto-')).length, 40);
  assert.ok(!left.includes(`eval-auto-${stamp(2)}.txt`) && left.includes(`eval-auto-${stamp(3)}.txt`), 'the three oldest went');
  assert.ok(left.includes('eval-2026-09-01-10-00.txt') && left.includes('latest.json'));
});

test('automatic evaluation (0.16.0): while the plan says a quick update, one starts after as many new games as in 0.15 (300, or 3 % of those the last one saw), at most an hour after it', async () => {
  const h = harness({ evaluatedAt: 100 * H - 30 * MIN });
  h.db.add(100_000 - h.db.rows.length);
  const fullAt = 100 * H - 3 * H;
  h.state.summary = {
    ...h.state.summary!, path: 'update', mark: { row: 100_000, games: 100_000, patch: '16.19' },
    full: { evaluatedAt: fullAt, mark: { row: 99_000, games: 99_000, patch: '16.19' } },
  };
  const reason = '300 new games since the last evaluation (full evaluation of 21 Oct)';
  h.state.plan = { path: 'update', reason, newGames: 300, sinceFull: 1_300, fullAt };
  h.db.add(299);
  h.advance(11 * MIN);
  h.auto.check();
  const s = () => h.auto.status();
  assert.deepEqual([s().path, s().reason, s().needed, s().updateGames, s().newGames, s().sinceFull, h.started.length], ['update', reason, 3_000, 3_000, 299, 1_299, 0],
    '3 % of the 100,000 games the last one saw');
  h.db.add(2_701);
  h.auto.check();
  assert.deepEqual([s().waiting, s().notBefore], ['spacing', 100 * H + 30 * MIN], 'an hour after the last evaluation');
  h.advance(19 * MIN);
  h.auto.check();
  assert.deepEqual([h.started, h.paths], [['3,000 new games'], ['update']]);

  // It fails: the next try an hour later, not six.
  await h.fail('The evaluation stopped');
  const failedAt = h.now;
  h.advance(MIN);
  h.auto.check();
  assert.deepEqual([s().waiting, s().notBefore], ['spacing', failedAt + H]);
});

test("automatic evaluation (0.16.0): when the plan says full, games since the last full evaluation count (300, or 3 % of those it saw), and it runs 6 hours after that one at the earliest: a quick update since doesn't put it off", async () => {
  const h = harness();
  h.db.add(20_500 - h.db.rows.length);
  const fullAt = h.now - 2 * H;
  // The last evaluation: a quick update half an hour ago, on a full evaluation of 20,000 games two hours ago.
  h.state.summary = {
    ...h.state.summary!, evaluatedAt: h.now - 30 * MIN, path: 'update', mark: { row: 20_400, games: 20_400, patch: '16.19' },
    full: { evaluatedAt: fullAt, mark: { row: 20_000, games: 20_000, patch: '16.19' } },
  };
  h.state.plan = { path: 'full', reason: 'settings the evaluation depends on changed', newGames: 100, sinceFull: 500, fullAt };
  h.advance(11 * MIN);
  h.auto.check();
  const s = () => h.auto.status();
  assert.deepEqual([s().path, s().newGames, s().sinceFull, s().needed, s().waiting], ['full', 100, 500, 600, null], 'not due: 3 % of 20,000');
  h.db.add(100);
  h.auto.check();
  assert.deepEqual([s().waiting, s().notBefore, h.started.length], ['spacing', fullAt + 6 * H, 0], 'from the full evaluation, not the quick update since');
  h.advance(fullAt + 6 * H - h.now);
  h.auto.check();
  assert.deepEqual([h.started, h.paths], [['600 games since the last full evaluation'], ['full']]);

  await h.fail('The evaluation stopped');
  const failedAt = h.now;
  h.advance(2 * H);
  h.auto.check();
  assert.deepEqual([s().waiting, s().notBefore], ['spacing', failedAt + 6 * H], 'a failed full evaluation: 6 hours');
});

test('automatic evaluation (0.16.0): the plan is worked out at each check, not while an evaluation runs or the data folder moves; up to date, nothing starts; a plan that fails keeps the last', async () => {
  const h = harness();
  h.state.plan = { path: 'none', reason: 'up to date: no new games since the last evaluation', newGames: 0, sinceFull: 0, fullAt: 1 };
  h.db.add(400);
  h.advance(11 * MIN);
  h.auto.check();
  const s = () => h.auto.status();
  assert.deepEqual([s().path, s().reason, s().waiting, h.started.length], ['none', 'up to date: no new games since the last evaluation', null, 0]);

  // Off: the plan changing is still pushed (the Data tab shows it).
  h.state.following = false;
  h.auto.check();
  const pushed = h.statuses.length;
  h.state.plan = { path: 'update', reason: '400 new games since the last evaluation', newGames: 400, sinceFull: 400, fullAt: 1 };
  h.auto.check();
  assert.deepEqual([h.statuses.length, h.auto.plan()?.path, h.started.length], [pushed + 1, 'update', 0]);

  h.state.following = true;
  h.auto.check();
  assert.deepEqual(h.paths, ['update']);
  const plans = h.plans;
  h.auto.check();
  assert.equal(h.plans, plans, 'not while it runs: its base is being written');
  await h.finish({ evaluatedAt: h.now, path: 'update', mark: { row: 1400, games: 1400, patch: '16.19' } });
  assert.equal(h.plans, plans + 1, 'once it ends');

  h.state.busy = true;
  h.auto.check();
  assert.equal(h.plans, plans + 1, 'not while the data folder moves');
  h.state.busy = false;
  h.state.planError = 'scrape.config.json could not be read';
  h.auto.check();
  assert.deepEqual([h.plans, h.auto.plan()?.path], [plans + 2, 'update']);
});

test('auto-eval: on a large database, 3 % of the games the last evaluation saw (0.15.0)', () => {
  const h = harness();
  h.db.add(100_000 - h.db.rows.length);
  h.state.summary = { ...h.state.summary!, mark: { row: 100_000, games: 100_000, patch: '16.19' } };
  h.db.add(2_999);
  h.auto.check();
  assert.deepEqual([h.auto.status().needed, h.auto.status().newGames, h.started.length], [3_000, 2_999, 0], 'not due at 2,999');
  h.db.add(1);
  h.advance(11 * MIN);
  h.auto.check();
  assert.equal(h.auto.status().newGames, 3_000);
  assert.ok(h.started.length === 1 || h.auto.status().waiting !== null, 'due at 3,000');
});

test('automatic evaluation: quick updates\' reports are pruned by their time among the full ones (0.16.0)', () => {
  const dir = path.join(root, 'evals-mixed');
  fs.mkdirSync(dir, { recursive: true });
  const stamp = (i: number) => `2026-11-${String(1 + i).padStart(2, '0')}-10-00`;
  // Old quick updates first, then newer full ones: the old quick updates go, not the newer full reports.
  for (let i = 0; i < 22; i++) for (const ext of ['txt', 'json']) fs.writeFileSync(path.join(dir, `eval-auto-${i < 5 ? 'update-' : ''}${stamp(i)}.${ext}`), '');
  pruneAutoReports(dir);
  const left = fs.readdirSync(dir);
  assert.equal(left.length, 40);
  assert.ok(!left.includes(`eval-auto-update-${stamp(1)}.txt`) && left.includes(`eval-auto-update-${stamp(2)}.txt`), 'the two oldest went');
  assert.ok(left.includes(`eval-auto-${stamp(5)}.txt`), 'the full reports after them stay');
});
