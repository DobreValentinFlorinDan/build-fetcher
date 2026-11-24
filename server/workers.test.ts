// The worker threads (server/workers.ts) on a real data folder: the model worker answers before and after its first
// build, and the model and scraper workers start again by themselves after an unexpected exit, so a request or a
// Start / Stop never talks to a worker that is gone. The job thread reports its progress and can be cancelled.
// Offline: Data Dragon comes from the tests' snapshot.
import assert from 'node:assert/strict';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import type { Worker } from 'node:worker_threads';
import type { DraftResponse, JobsStatus, ModelStatus, ScraperStatus } from '../shared/types.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-workers-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');
const dataDir = path.join(root, 'data');

const { captureTimeline, participantRows, CAPTURE_VERSION } = await import('./capture.ts');
const { openDb } = await import('./db.ts');
const { captureGame } = await import('./scraper/ingest.ts');
const { AP_TEAM, apAdGames, synthMatch } = await import('./test/modelFixtures.ts');
const { loadTestStaticData } = await import('./test/staticData.ts');
const { ModelClient } = await import('./model/client.ts');
const { ScraperController } = await import('./scraper/controller.ts');
const { CANCELLED, Jobs } = await import('./jobs.ts');

// The snapshot as Data Dragon's cache in the data folder: the workers load it from there instead of the network.
const snap = JSON.parse(fs.readFileSync(new URL('./test/ddragon-snapshot.json', import.meta.url), 'utf8'));
const cacheDir = path.join(dataDir, 'ddragon', snap.version);
fs.mkdirSync(cacheDir, { recursive: true });
for (const [file, raw] of [['item.json', snap.raw.item], ['champion.json', snap.raw.champion], ['runesReforged.json', snap.raw.runesReforged],
  ['summoner.json', snap.raw.summoner], ['Irelia.json', snap.raw.champ]]) fs.writeFileSync(path.join(cacheDir, file), JSON.stringify(raw));

const data = loadTestStaticData();
const db = openDb(path.join(dataDir, 'irelia.db'));
db.tx(() => {
  for (const s of apAdGames(120, 30)) {
    const { match, timeline } = synthMatch(s);
    const cap = captureTimeline(timeline);
    const { row } = captureGame(match, timeline, match.info.participants[0], 'otp', undefined);
    db.insertMatch({ id: s.id, platform: 'KR', patch: data.patch, version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
    db.insertGame(row);
    db.insertParticipants(s.id, participantRows(match));
    db.insertTimeline(s.id, cap.interval, JSON.stringify(cap));
    db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
  }
});

const boot = () => ({ dataDir, versions: [snap.version] });
const workerOf = (x: object) => (x as { worker: Worker | null }).worker;
const toClose: { terminate(): Promise<void> }[] = [];
after(async () => {
  await Promise.all(toClose.map((x) => x.terminate()));
  db.close();
  fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 });
});

/** Resolves with the first event that passes `ok`. */
function until<T>(emitter: NodeJS.EventEmitter, event: string, ok: (v: T) => boolean, ms = 30_000) {
  return new Promise<T>((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`no ${event} in ${ms} ms`)), ms);
    const on = (v: T) => { if (ok(v)) { clearTimeout(t); emitter.off(event, on); resolve(v); } };
    emitter.on(event, on);
  });
}

test('model worker: builds on its own, answers drafts from the model, and comes back after an unexpected exit', async () => {
  const models = new ModelClient(boot);
  toClose.push(models);
  await until<ModelStatus>(models, 'status', (s) => s.state === 'ready');
  const q = { lane: 'TOP', opponent: null, enemies: AP_TEAM, allies: [], regions: null };
  const r = await models.request<DraftResponse>('draft', q);
  assert.ok(r.baseline.sample.totalGames > 0, 'the lane baseline');
  assert.ok(r.draft, 'and the draft-adjusted build');
  assert.equal(r.model?.games, 120);

  // The worker dies. A request in flight never hangs: it is answered if the worker got to it first, else it fails
  // with "stopped" (which one depends on the machine's timing). Until the new worker is up, requests fail at once.
  const old = workerOf(models)!;
  const exited = until<ModelStatus>(models, 'status', (s) => s.state === 'loading');
  const pending = models.request('status').then(() => 'answered', (e: Error) => e.message);
  await old.terminate();
  await exited;
  const outcome = await Promise.race([pending, new Promise((r) => setTimeout(() => r('hung'), 2000))]);
  assert.ok(outcome === 'answered' || /stopped/.test(String(outcome)), `in flight: ${outcome}`);
  await assert.rejects(models.request('status'), /restarting/);
  await until<ModelStatus>(models, 'status', (s) => s.state === 'ready');
  assert.notEqual(workerOf(models), old);
  assert.ok((await models.request<DraftResponse>('draft', q)).draft);
});

test('scraper worker: comes back by itself after an unexpected exit, and Stop is answered at once', async () => {
  const scraper = new ScraperController(data.patch, () => dataDir, data.previousPatch);
  toClose.push(scraper);
  await until<ScraperStatus>(scraper, 'status', () => true);
  const old = workerOf(scraper)!;
  await old.terminate();
  await until<ScraperStatus>(scraper, 'status', () => true);
  assert.ok(workerOf(scraper) && workerOf(scraper) !== old, 'a new worker');

  const t0 = Date.now();
  scraper.stop();
  const status = await until<ScraperStatus>(scraper, 'status', (s) => !s.running);
  assert.equal(status.running, false);
  assert.ok(Date.now() - t0 < 2000, 'answered at once');

  // Paused for a data-folder move: no worker, and none comes back until it is resumed.
  await scraper.terminate();
  assert.equal(workerOf(scraper), null);
  scraper.stop();
  assert.equal(workerOf(scraper), null, 'Stop does not start one while paused');
  scraper.respawn();
  await once(scraper, 'status');
});

test('scraper worker: Pause fetching during games stops fetching in champ select and resumes it after, and its garbage is collected on request', async () => {
  // A key that looks right (nothing is fetched while paused; the lanes are stopped again before they get far).
  const scraper = new ScraperController(data.patch, () => dataDir, data.previousPatch, () => 'RGAPI-00000000-0000-0000-0000-000000000000');
  toClose.push(scraper);
  await until<ScraperStatus>(scraper, 'status', () => true);
  scraper.setInGame(true, true);
  scraper.start();
  const paused = await until<ScraperStatus>(scraper, 'status', (s) => s.paused === 'game');
  assert.equal(paused.running, false, 'no lanes in champ select');
  assert.match(paused.phase, /Paused during champ select/);
  await scraper.collectGarbage(5000);
  scraper.setInGame(false);
  const resumed = await until<ScraperStatus>(scraper, 'status', (s) => s.running);
  assert.equal(resumed.paused, undefined);
  // In a game with the setting off: fetching goes on.
  scraper.setInGame(true, false);
  scraper.setInGame(true, false);
  scraper.stop();
  const stopped = await until<ScraperStatus>(scraper, 'status', (s) => !s.running);
  assert.equal(stopped.paused, undefined);
  // Paused again, then stopped: it doesn't come back after the game.
  scraper.setPauseInGame(true);
  scraper.setInGame(false);
  const idle = await until<ScraperStatus>(scraper, 'status', (s) => !s.running && s.paused === undefined);
  assert.equal(idle.running, false);
});

test('job thread: the evaluation reports each step and its result, and Cancel stops it', async () => {
  const jobs = new Jobs(boot);
  const seen: JobsStatus[] = [];
  jobs.on('status', (s: JobsStatus) => seen.push(s));
  const result = await jobs.run({ job: 'evaluate', quick: true });
  assert.match(result.text, /Gate: FAILED/, 'too few games for the gate');
  const steps = seen.flatMap((s) => s.running.map((r) => r.step)).filter((x) => x !== null);
  assert.deepEqual(steps, [...steps].sort((a, b) => a - b), 'steps only move forward');
  assert.equal(steps[0], 1);
  assert.equal(steps.at(-1), seen.flatMap((s) => s.running).at(-1)!.steps, 'up to the last step');
  // How far it is (0.10.0): from 0, only forward, never past the end; the time left needs a size (none here).
  const fractions = seen.flatMap((s) => s.running.map((r) => r.progress?.fraction ?? null));
  assert.ok(fractions.length && fractions.every((f) => f !== null && f >= 0 && f <= 1), `fractions ${fractions.join(',')}`);
  assert.deepEqual(fractions, [...fractions].sort((a, b) => a! - b!), 'only forward');
  assert.deepEqual(jobs.status.running, []);
  assert.equal(jobs.status.last.evaluate?.ok, true);
  assert.equal(jobs.status.last.evaluate?.text, result.text);
  // Its threads (0.11.0): as many as the cores and the memory allow, said while it runs and kept with its result.
  const threads = seen.flatMap((s) => s.running.map((r) => r.threads)).filter((x) => x !== undefined);
  assert.ok(threads.length && threads.every((t) => t! >= 1), `threads ${threads.join(',')}`);
  assert.equal(jobs.status.last.evaluate?.threads, threads.at(-1));

  // Asked again while it runs: the same run. Cancelled: its callers get "Cancelled", and the status says so.
  const first = jobs.run({ job: 'evaluate' });
  const second = jobs.run({ job: 'evaluate' });
  assert.equal(jobs.status.running.length, 1);
  await jobs.cancel('evaluate');
  await assert.rejects(first, { message: CANCELLED });
  await assert.rejects(second, { message: CANCELLED });
  assert.deepEqual(jobs.status.running, []);
  assert.equal(jobs.status.last.evaluate?.cancelled, true);
  assert.equal(jobs.status.last.evaluate?.text, undefined);
});

test('job as a process (0.22.0): the evaluation runs in a process of its own, reports its steps and result, and Cancel stops it', async () => {
  const was = process.env.IRELIA_EVAL_PROCESSES;
  process.env.IRELIA_EVAL_PROCESSES = '1';
  try {
    const jobs = new Jobs(boot);
    const seen: JobsStatus[] = [];
    jobs.on('status', (s: JobsStatus) => seen.push(s));
    // Champ select during it: the process is told, and finishes all the same.
    jobs.setBusy(true);
    const result = await jobs.run({ job: 'evaluate', quick: true });
    jobs.setBusy(false);
    assert.match(result.text, /Gate: FAILED/);
    assert.match(result.text, /Time by step/);
    const steps = seen.flatMap((s) => s.running.map((r) => r.step)).filter((x) => x !== null);
    assert.ok(steps.length > 3 && steps[0] === 1, `steps ${steps.join(',')}`);
    assert.equal(jobs.status.last.evaluate?.ok, true);
    assert.equal(jobs.status.last.evaluate?.text, result.text);

    const first = jobs.run({ job: 'evaluate' });
    await new Promise((r) => setTimeout(r, 300));
    await jobs.cancel('evaluate');
    await assert.rejects(first, { message: CANCELLED });
    assert.deepEqual(jobs.status.running, []);
    assert.equal(jobs.status.last.evaluate?.cancelled, true);
  } finally {
    if (was === undefined) delete process.env.IRELIA_EVAL_PROCESSES; else process.env.IRELIA_EVAL_PROCESSES = was;
  }
});

test('job as a process: one that can\'t start runs as a thread instead, with the same kind of result (0.22.0)', async () => {
  const was = [process.env.IRELIA_EVAL_PROCESSES, process.env.IRELIA_TEST_JOB_PROCESS_FAILS];
  process.env.IRELIA_EVAL_PROCESSES = '1';
  process.env.IRELIA_TEST_JOB_PROCESS_FAILS = '1';
  const warned: string[] = [];
  const warn = console.warn;
  console.warn = (...a: unknown[]) => { warned.push(a.join(' ')); };
  try {
    const result = await new Jobs(boot).run({ job: 'evaluate', quick: true });
    assert.match(result.text, /Gate: FAILED/);
    assert.ok(warned.some((w) => /process ended before it began \(exit 3\): it runs as a thread instead/.test(w)), warned.join('\n'));
  } finally {
    console.warn = warn;
    if (was[0] === undefined) delete process.env.IRELIA_EVAL_PROCESSES; else process.env.IRELIA_EVAL_PROCESSES = was[0];
    delete process.env.IRELIA_TEST_JOB_PROCESS_FAILS;
  }
});

test('job thread: an automatic evaluation says why, saves its report as eval-auto-… with what it saw, can be paused, and a click joining it makes it yours', async () => {
  const jobs = new Jobs(boot);
  const result = await jobs.run({ job: 'evaluate', quick: true, auto: '300 new games' });
  assert.match(path.basename(result.file), /^eval-auto-/);
  assert.equal(jobs.status.last.evaluate?.auto, '300 new games');
  const summary = JSON.parse(fs.readFileSync(path.join(path.dirname(result.file), 'latest.json'), 'utf8'));
  assert.deepEqual(summary.mark, { ...db.gameMark(), patch: data.patch });

  const paused = jobs.run({ job: 'evaluate', auto: 'new patch 16.20' });
  assert.equal(jobs.status.running[0].auto, 'new patch 16.20');
  await jobs.cancel('evaluate', true);
  await assert.rejects(paused, { message: CANCELLED });
  assert.deepEqual([jobs.status.last.evaluate?.paused, jobs.status.last.evaluate?.auto], [true, 'new patch 16.20']);

  const auto = jobs.run({ job: 'evaluate', auto: '300 new games' });
  const mine = jobs.run({ job: 'evaluate' });
  assert.equal(jobs.status.running[0].auto, undefined, 'a click makes it yours');
  assert.equal(jobs.runningJob('evaluate')?.auto, undefined);
  await jobs.cancel('evaluate');
  await assert.rejects(mine, { message: CANCELLED });
  await assert.rejects(auto, { message: CANCELLED });
  assert.deepEqual([jobs.status.last.evaluate?.auto, jobs.status.last.evaluate?.paused], [undefined, undefined]);
});

test('job thread: a full evaluation asked for while a quick update runs replaces it; anything else joins (0.16.0)', async () => {
  const jobs = new Jobs(boot);
  const quick = jobs.run({ job: 'evaluate', auto: '300 new games', path: 'auto', planned: 'update' });
  assert.equal(jobs.status.running[0].path, 'update');
  const joined = jobs.run({ job: 'evaluate', path: 'auto' });
  assert.equal(jobs.status.running.length, 1, 'an automatic request joins it');
  const full = jobs.run({ job: 'evaluate', path: 'full' });
  await assert.rejects(quick, { message: CANCELLED });
  await assert.rejects(joined, { message: CANCELLED });
  assert.equal(jobs.status.running.length, 1);
  assert.notEqual(jobs.status.running[0].path, 'update', 'the full one runs instead');
  const again = jobs.run({ job: 'evaluate', path: 'full' });
  assert.equal(jobs.status.running.length, 1, 'a second full request joins the full one');
  await jobs.cancel('evaluate');
  await assert.rejects(full, { message: CANCELLED });
  await assert.rejects(again, { message: CANCELLED });
});
