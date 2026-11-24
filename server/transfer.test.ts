// Backing up the pool (0.18.0): ZIP export and import of the pool, other players' IDs left out, and keeping only the
// newest exports.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { openDb, type Db } from './db.ts';
import { FakeRiot, IRELIA, makeMatch } from './test/fixtures.ts';
import { parseScope } from './scrapeScope.ts';
import { fetchMatch, storeMatch } from './scraper/ingest.ts';
import { Sourcing } from './scraper/sourcing.ts';
import { exportFileName, exportOwner, exportPool, importExport, inspectExport, POOL_EXPORT, pruneExports } from './transfer.ts';
import { ZipWriter } from './zip.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-transfer-'));
const opened: Db[] = [];
after(() => {
  // Windows can't delete a database file that is still open.
  for (const db of opened) try { db.close(); } catch { /* closed by its test */ }
  fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
});

const NOW = Date.now();
const DAY = 24 * 3600_000;
const signal = new AbortController().signal;
const quiet = () => {};

/** A pool (its own folder) with `games` Irelia games of the one-trick P, and a ladder player no game mentions. */
async function pool(dir: string, games: number) {
  const dataDir = path.join(root, dir);
  const db = openDb(path.join(dataDir, 'irelia.db'));
  opened.push(db);
  const riot = new FakeRiot();
  const s = new Sourcing({
    db, riot, scope: () => parseScope({ platforms: ['KR'], tiers: ['CHALLENGER'] }), fallbackPatch: () => ({ patch: '16.19', previousPatch: '16.18' }),
    me: () => null, concurrency: 1, log: () => {}, now: () => NOW,
  });
  // Brackets the KR patch, as the sourcing tests do.
  db.insertMatch({ id: 'KR_100', platform: 'KR', patch: '16.18', version: '16.18.700.1', queue: 420, creation: NOW - 12 * DAY, duration: 1800, hasIrelia: false });
  db.insertMatch({ id: 'KR_101', platform: 'KR', patch: '16.19', version: '16.19.712.4471', queue: 420, creation: NOW - 8 * DAY, duration: 1800, hasIrelia: false });
  db.upsertPlayer('P', 'KR', 'CHALLENGER', 900);
  db.setPilot('P', { pilotClass: 'otp', ireliaPoints: 800_000, ireliaLastPlay: NOW, ireliaRank: 1 });
  db.upsertPlayer('LADDER-ONLY', 'KR', 'MASTER', 1);
  for (let i = 0; i < games; i++) {
    const id = `KR_${500 + i}`;
    const champions = [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89];
    const puuids = champions.map((_, k) => (k === 0 ? 'P' : `${id}-p${k + 1}`));
    riot.add(makeMatch({ id, champions, puuids, creation: NOW - (0.5 + i * 0.3) * DAY, version: '16.19.712.4471' }));
    assert.equal(storeMatch(db, id, await fetchMatch(s.ingestEnv(), 'KR', id, { pilotPuuid: 'P', source: 'otp' }, signal)), 'irelia');
  }
  return { dataDir, db };
}

test('the pool exported on one computer and imported on another: same games, other players stripped, nothing replaced', async () => {
  const a = await pool('pc-a', 6);
  const out = path.join(root, 'drive');
  const ex = await exportPool(a.dataDir, out, { app: 'test', progress: quiet });
  assert.equal(path.basename(ex.file), exportFileName(POOL_EXPORT, ex.exportedAt));
  assert.equal(ex.games, 6);
  assert.ok(!fs.readdirSync(out).some((f) => f.includes('.partial') || f.startsWith('.irelia-transfer')), 'nothing half-written left');
  const { manifest, entries } = await inspectExport(ex.file);
  assert.deepEqual([manifest.kind, manifest.name, manifest.stripped], ['pool', POOL_EXPORT, true]);
  assert.deepEqual(entries.map((e) => e.name).sort(), ['irelia-pool.db', 'manifest.json']);

  // Another computer: a fresh pool. Importing changes its builds, so it has to be confirmed.
  const bDir = path.join(root, 'pc-b');
  openDb(path.join(bDir, 'irelia.db')).close();
  await assert.rejects(importExport(bDir, ex.file, { progress: quiet }), /confirm it first/);
  const r = await importExport(bDir, ex.file, { confirmed: true, progress: quiet });
  assert.deepEqual([r.name, r.added], [POOL_EXPORT, 6]);
  const b = openDb(path.join(bDir, 'irelia.db'), { migrate: false });
  opened.push(b);
  assert.equal(b.ireliaMatchIds().length, 6);
  assert.equal(b.player('P')?.pilot_class, 'otp', 'the pilot it follows comes along');
  assert.equal(b.player('LADDER-ONLY'), undefined, 'ladder players that are not pilots stay behind');
  const ids = new Set((b.raw.prepare('SELECT DISTINCT puuid FROM participants').all() as { puuid: string }[]).map((x) => x.puuid));
  assert.deepEqual([...ids].sort(), ['', 'P'], 'only the pilot keeps an ID');
  // The original keeps every ID: stripping happens on the export copy only.
  assert.ok((a.db.raw.prepare('SELECT COUNT(DISTINCT puuid) AS n FROM participants').get() as { n: number }).n > 2);

  // Importing again adds nothing.
  assert.equal((await importExport(bDir, ex.file, { confirmed: true, progress: quiet })).added, 0);
});

test('two computers blend their pools: each import adds only the games the other has, both ways (0.20.1)', async () => {
  // The same pilot's games: KR_500 to KR_505 on the first computer, KR_500 to KR_508 on the second (6 in common).
  const a = await pool('blend-a', 6);
  const c = await pool('blend-c', 9);
  const drive = path.join(root, 'blend-drive');
  const fromC = await exportPool(c.dataDir, drive, { app: 'test', progress: quiet });
  a.db.close();
  assert.equal((await importExport(a.dataDir, fromC.file, { confirmed: true, progress: quiet })).added, 3, 'only the 3 games the first computer lacked');
  const a2 = openDb(path.join(a.dataDir, 'irelia.db'), { migrate: false });
  opened.push(a2);
  assert.equal(a2.ireliaMatchIds().length, 9);
  // Back the other way: nothing new for the second computer, and nothing it had is touched.
  const fromA = await exportPool(a.dataDir, path.join(root, 'blend-drive-2'), { app: 'test', progress: quiet });
  c.db.close();
  assert.equal((await importExport(c.dataDir, fromA.file, { confirmed: true, progress: quiet })).added, 0);
  const c2 = openDb(path.join(c.dataDir, 'irelia.db'), { migrate: false });
  opened.push(c2);
  assert.equal(c2.ireliaMatchIds().length, 9);
});

test('only the newest exports are kept, and only files named like ours are touched', async () => {
  const dir = path.join(root, 'prune');
  fs.mkdirSync(dir);
  for (let i = 0; i < 5; i++) fs.writeFileSync(path.join(dir, exportFileName(POOL_EXPORT, Date.UTC(2026, 9, 1 + i, 12))), 'x');
  fs.writeFileSync(path.join(dir, `${POOL_EXPORT} notes.zip`), 'mine');
  fs.writeFileSync(path.join(dir, 'holiday.zip'), 'mine');
  assert.equal(await pruneExports(dir, POOL_EXPORT, 3), 2);
  const left = fs.readdirSync(dir).sort();
  assert.equal(left.filter((f) => f.startsWith('irelia-pool_')).length, 3);
  assert.ok(left.includes(`${POOL_EXPORT} notes.zip`) && left.includes('holiday.zip'));
  assert.ok(!left.includes(exportFileName(POOL_EXPORT, Date.UTC(2026, 9, 1, 12))), 'the oldest went');
  assert.equal(exportOwner(exportFileName(POOL_EXPORT, NOW)), POOL_EXPORT);
  assert.equal(exportOwner('holiday.zip'), null);
  assert.equal(exportOwner(exportFileName('Agurin-EUW_dataset', NOW)), null, '0.18.0 data set exports are not ours any more');
});

test('two computers backing up into one folder each keep their own newest three (0.20.1)', async () => {
  const dir = path.join(root, 'shared-folder');
  fs.mkdirSync(dir);
  const write = async (machine: string, day: number) => {
    const file = path.join(dir, exportFileName(POOL_EXPORT, Date.UTC(2026, 10, day, 12)));
    const w = new ZipWriter(file);
    await w.addBuffer('manifest.json', JSON.stringify({ format: 'irelia-companion-export', version: 1, kind: 'pool', name: POOL_EXPORT, machine, schema: 1, database: 'x.db' }));
    await w.addBuffer('x.db', 'x');
    await w.finish();
    return path.basename(file);
  };
  // The large pool's computer backed up once, long ago; the new one four times since.
  const big = await write('BIG-PC', 1);
  const fresh = [await write('NEW-PC', 2), await write('NEW-PC', 3), await write('NEW-PC', 4), await write('NEW-PC', 5)];
  fs.writeFileSync(path.join(dir, exportFileName(POOL_EXPORT, Date.UTC(2026, 9, 1, 12))), 'not a zip');
  assert.equal(await pruneExports(dir, POOL_EXPORT, 3, 'NEW-PC'), 1);
  const left = fs.readdirSync(dir);
  assert.ok(left.includes(big), "the other computer's backup stays");
  assert.ok(!left.includes(fresh[0]) && fresh.slice(1).every((f) => left.includes(f)), "this computer's newest three stay");
  assert.ok(left.includes(exportFileName(POOL_EXPORT, Date.UTC(2026, 9, 1, 12))), 'a file it can\'t read is left alone');
});

test('a 0.18.0 player data set export says it can no longer be imported', async () => {
  const file = path.join(root, 'old-dataset.zip');
  const w = new ZipWriter(file);
  await w.addBuffer('manifest.json', JSON.stringify({ format: 'irelia-companion-export', version: 1, kind: 'dataset', name: 'Agurin-EUW_dataset', schema: 1, database: 'x.db' }));
  await w.addBuffer('x.db', 'x');
  await w.finish();
  await assert.rejects(inspectExport(file), /no longer imports/);
  await assert.rejects(inspectExport(path.join(root, 'nothing.zip')));
});
