// npm run eval:builds and the app's Evaluate builds (model/runEval.ts) on a real database, 0.20.0: every report ends
// with the time each step took; a grouped-drafts trial is saved beside the reports as eval-grouped-… and changes
// nothing the app reads (latest.json, the quick update's base, the evaluation history).
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-run-eval-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');
process.env.IRELIA_DATA_DIR = path.join(root, 'data');
fs.mkdirSync(path.join(root, 'data'), { recursive: true });

const { evaluateDataFolder } = await import('./runEval.ts');
const { compareReports } = await import('./evalCompare.ts');
const { evalHistory } = await import('./evalHistory.ts');
const { evalDir } = await import('./service.ts');
const { captureTimeline, participantRows, CAPTURE_VERSION } = await import('../capture.ts');
const { openDb } = await import('../db.ts');
const { captureGame } = await import('../scraper/ingest.ts');
const { apAdGames, synthMatch } = await import('../test/modelFixtures.ts');
const { loadTestStaticData } = await import('../test/staticData.ts');

after(() => { fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

test('a grouped trial is saved beside the reports and changes nothing the app reads; every report gives its time by step', async () => {
  const db = openDb(path.join(root, 'data', 'irelia.db'));
  try {
    const data = loadTestStaticData();
    const patch = data.version.split('.').slice(0, 2).join('.');
    db.tx(() => {
      for (const s of apAdGames(400, 40, 9, { effect: true, randomOpponent: true }).map((g) => ({ ...g, version: `${patch}.1.1` }))) {
        const { match, timeline } = synthMatch(s);
        const { row } = captureGame(match, timeline, match.info.participants[0], 'otp', undefined);
        db.insertMatch({ id: s.id, platform: 'KR', patch, version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
        db.insertGame(row);
        db.insertParticipants(s.id, participantRows(match));
        db.insertTimeline(s.id, captureTimeline(timeline).interval, JSON.stringify(captureTimeline(timeline)));
        db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
      }
    });
    const exact = await evaluateDataFolder(db, data, { quick: true, threads: 1 });
    assert.ok(exact.report.games > 0, 'the games were read');
    assert.match(exact.text, /Time by step \(0\.20\.0\)/);
    assert.equal(exact.timings[0].label, 'Reading the stored games');
    const dir = evalDir();
    const latest = fs.readFileSync(path.join(dir, 'latest.json'), 'utf8');
    const saved = JSON.parse(fs.readFileSync(exact.file.replace(/\.txt$/, '.json'), 'utf8'));
    assert.ok(Array.isArray(saved.timings) && saved.timings.length > 2, 'the JSON report has its timings');

    const grouped = await evaluateDataFolder(db, data, { quick: true, threads: 1, grouping: 0 });
    assert.ok(grouped.report.grouping && grouped.report.grouping.groups > 1);
    assert.match(path.basename(grouped.file), /^eval-grouped-/);
    assert.match(grouped.text, /Grouped drafts \(a trial/);
    assert.equal(fs.readFileSync(path.join(dir, 'latest.json'), 'utf8'), latest, 'latest.json is the exact evaluation\'s');
    assert.equal(evalHistory(dir).length, 1, 'the history shows the exact evaluation only');
    assert.equal(grouped.report.games, exact.report.games);

    const text = compareReports({ report: exact.report, ms: exact.ms }, { report: grouped.report, ms: grouped.ms });
    assert.match(text, /Draft model gate: (passed|failed) → (passed|failed)/);
    assert.match(text, /Summary: /);
  } finally { db.close(); }
});
