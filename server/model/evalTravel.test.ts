// An evaluation that travels with a backup (0.21.0, model/evalTravel.ts), on real databases: a computer evaluates its
// pool and backs it up; another one, with games of its own, imports the backup, takes the evaluation (its verdicts
// apply at once) and its next evaluation is a quick update measuring only its own games, not a full one. A second
// import of an evaluation covering no more games is turned down.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-eval-travel-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');
process.env.IRELIA_DATA_DIR = path.join(root, 'pc-a');
fs.mkdirSync(path.join(root, 'pc-a'), { recursive: true });

const { setDataDir } = await import('../config.ts');
const { evaluateDataFolder } = await import('./runEval.ts');
const { planEvaluation, readBaseHeader } = await import('./evalBase.ts');
const { adoptIncomingEvaluation, incomingEvaluation } = await import('./evalTravel.ts');
const { evalDir, readEvaluation } = await import('./service.ts');
const { exportPool, importExport, inspectExport } = await import('../transfer.ts');
const { captureTimeline, participantRows, CAPTURE_VERSION } = await import('../capture.ts');
const { openDb } = await import('../db.ts');
const { captureGame } = await import('../scraper/ingest.ts');
const { apAdGames, synthMatch } = await import('../test/modelFixtures.ts');
const { loadTestStaticData } = await import('../test/staticData.ts');
type Db = import('../db.ts').Db;

after(() => { fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

const data = loadTestStaticData();
const patch = data.version.split('.').slice(0, 2).join('.');
const quiet = () => {};

function store(db: Db, games: ReturnType<typeof apAdGames>) {
  db.tx(() => {
    for (const s of games.map((g) => ({ ...g, version: `${patch}.1.1` }))) {
      const { match, timeline } = synthMatch(s);
      const { row } = captureGame(match, timeline, match.info.participants[0], 'otp', undefined);
      db.insertMatch({ id: s.id, platform: 'KR', patch, version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
      db.insertGame(row);
      db.insertParticipants(s.id, participantRows(match));
      db.insertTimeline(s.id, captureTimeline(timeline).interval, JSON.stringify(captureTimeline(timeline)));
      db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
    }
  });
}

test('a backup carries the evaluation; the computer importing it takes it and only measures its own games next', async () => {
  // Computer A: its pool, evaluated in full, then backed up.
  const aDir = path.join(root, 'pc-a');
  const a = openDb(path.join(aDir, 'irelia.db'));
  store(a, apAdGames(400, 40, 9, { effect: true, randomOpponent: true }));
  const full = await evaluateDataFolder(a, data, { threads: 1 });
  assert.equal(full.path, 'full');
  assert.ok(readBaseHeader()?.portable, 'the base has its portable key');
  a.close();
  const drive = path.join(root, 'drive');
  const ex = await exportPool(aDir, drive, { app: 'test', progress: quiet });
  assert.equal(ex.evaluation, true);
  const { manifest } = await inspectExport(ex.file);
  assert.ok(manifest.evaluation && manifest.evaluation.known > 0, 'the manifest says it carries an evaluation');

  // Computer B: its own settings folder (nothing evaluated) and a fresh pool of 40 games A never had.
  fs.rmSync(evalDir(), { recursive: true, force: true });
  const bDir = path.join(root, 'pc-b');
  fs.mkdirSync(bDir, { recursive: true });
  setDataDir(bDir);
  let b = openDb(path.join(bDir, 'irelia.db'));
  store(b, apAdGames(40, 8, 77, { effect: true, randomOpponent: true }).map((g, i) => ({ ...g, id: `KR_${90_000 + i}`, pilot: `fresh${i % 8}` })));
  assert.equal(planEvaluation(b, data).path, 'full', 'before the import: never evaluated');
  b.close();
  const imported = await importExport(bDir, ex.file, { confirmed: true, progress: quiet });
  assert.equal(imported.added, 400);
  assert.equal(imported.evaluation, true);
  assert.ok(incomingEvaluation());

  b = openDb(path.join(bDir, 'irelia.db'));
  try {
    const r = adoptIncomingEvaluation(b, data)!;
    assert.equal(r.adopted, true, r.message);
    assert.match(r.message, /measures only the 40 games it didn't have/);
    assert.ok(!incomingEvaluation(), 'taken: nothing left waiting');
    const summary = readEvaluation()!;
    assert.equal(summary.gate, full.report.gate, "A's verdict applies on B at once");
    assert.equal(summary.importedFrom?.machine, os.hostname());
    assert.ok(fs.readdirSync(evalDir()).some((f) => /^eval-imported-.*\.txt$/.test(f)), 'its report is there to read');
    const plan = planEvaluation(b, data);
    assert.deepEqual([plan.path, plan.newGames], ['update', 40], plan.reason);

    // B's next evaluation: a quick update of its own 40 games, A's measurements kept.
    const next = await evaluateDataFolder(b, data, { threads: 1, path: 'auto' });
    assert.equal(next.path, 'update', next.reason);
    assert.ok(next.report.update && next.report.update.newGames > 0 && next.report.update.newGames <= 40, `measured ${next.report.update?.newGames}`);
    assert.equal(next.report.update.missing, 0);

    // The same backup again: B's own evaluation now covers as many games, so it is turned down.
    b.close();
    await importExport(bDir, ex.file, { confirmed: true, progress: quiet });
    b = openDb(path.join(bDir, 'irelia.db'));
    const again = adoptIncomingEvaluation(b, data)!;
    assert.equal(again.adopted, false);
    assert.match(again.message, /covers as many games or more/);
    assert.equal(readEvaluation()!.evaluatedAt, next.report.evaluatedAt, "B's own evaluation stays");
  } finally { b.close(); }
});
