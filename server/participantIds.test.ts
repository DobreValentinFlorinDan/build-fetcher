// Participants by integer ids (db.ts normalizeParticipants, 0.14.0): rebuilt only when they are most of the database
// and it comes out smaller; afterwards every reader reads the same rows through the view, and inserting, archiving,
// restoring and deleting matches work as before.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { participantRows } from './capture.ts';
import { openDb } from './db.ts';
import { IRELIA, makeMatch } from './test/fixtures.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-ids-'));
after(() => fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }));

/** `n` matches, their players drawn from `pool` long PUUIDs (high elo: the same players meet again and again). */
function fill(file: string, n: number, pool: number) {
  const db = openDb(file);
  const puuid = (k: number) => `${'x'.repeat(70)}${String(k).padStart(8, '0')}`;
  db.tx(() => {
    for (let i = 0; i < n; i++) {
      const id = `KR_${100_000 + i}`;
      const m = makeMatch({ id, champions: [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89], puuids: Array.from({ length: 10 }, (_, k) => puuid((i * 7 + k * 13) % pool)) });
      db.insertMatch({ id, platform: 'KR', patch: i % 2 ? '16.19' : '16.10', version: m.info.gameVersion, queue: 420, creation: m.info.gameCreation, duration: m.info.gameDuration, hasIrelia: false });
      db.insertParticipants(id, participantRows(m));
    }
  });
  return db;
}

const read = (db: ReturnType<typeof openDb>) => ({
  rows: db.raw.prepare('SELECT * FROM participants ORDER BY match_id, participant_id').all(),
  totals: db.championTotals(['16.19']),
  model: db.raw.prepare('SELECT match_id, participant_id, champion_id, team_id, position FROM participants WHERE match_id IN (SELECT match_id FROM matches WHERE patch = ?) ORDER BY 1, 2').all('16.19'),
  counts: db.rowCounts(),
});

test('participants by ids: the same rows for every reader, smaller, and writes, the archive and deletes work as before', () => {
  const file = path.join(root, 'ids.db');
  let db = fill(file, 300, 40);
  const before = read(db);
  const r = db.normalizeParticipants();
  assert.equal(r.done, true, JSON.stringify(r));
  assert.ok((r.after as number) < (r.before as number), `smaller: ${r.after} < ${r.before}`);
  db.close();
  db = openDb(file);
  assert.equal((db.raw.prepare("SELECT type FROM sqlite_master WHERE name = 'participants'").get() as { type: string }).type, 'view');
  assert.deepEqual(read(db), before, 'every reader reads the same');
  assert.equal(JSON.parse(db.getMeta('participants_layout')!).done, true);

  // A new match's players, and the same match stored again (idempotent).
  const m = makeMatch({ id: 'KR_999999', champions: [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89] });
  db.insertMatch({ id: 'KR_999999', platform: 'KR', patch: '16.19', version: m.info.gameVersion, queue: 420, creation: m.info.gameCreation, duration: m.info.gameDuration, hasIrelia: false });
  db.insertParticipants('KR_999999', participantRows(m));
  db.insertParticipants('KR_999999', participantRows(m));
  const of = (id: string) => db.raw.prepare('SELECT * FROM participants WHERE match_id = ? ORDER BY participant_id').all(id);
  assert.equal(of('KR_999999').length, 10);
  assert.deepEqual(of('KR_999999').map((x) => x.puuid), m.metadata.participants);

  // Archived and restored: the same rows.
  const kept = of('KR_100000');
  assert.ok(db.archiveBatch(['16.19']) > 0);
  assert.equal(of('KR_100000').length, 0, 'archived: out of the tables');
  while (db.restoreBatch(null) > 0) { /* all of them */ }
  assert.deepEqual(of('KR_100000'), kept, 'restored as they were');

  // Deleting a patch's matches takes their players with them (the trigger in place of ON DELETE CASCADE).
  db.purgeStale(['16.19']);
  assert.equal((db.raw.prepare("SELECT COUNT(*) AS n FROM participants WHERE match_id IN ('KR_100000', 'KR_100002')").get() as { n: number }).n, 0);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM participant_rows').get() as { n: number }).n, (db.raw.prepare('SELECT COUNT(*) AS n FROM participants').get() as { n: number }).n);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM match_ids WHERE match_id NOT IN (SELECT match_id FROM matches)').get() as { n: number }).n, 0, 'their ids go too');
  assert.deepEqual(db.normalizeParticipants(), { done: false, why: 'already by ids' });
  db.close();
});

test('participants by ids: not done when the table is a small part of the database, nor when it would not be smaller', () => {
  // Every PUUID different and short: by ids would only add an index.
  const file = path.join(root, 'unique.db');
  const db = fill(file, 300, 3000);
  const before = read(db);
  const r = db.normalizeParticipants();
  assert.equal(r.done, false);
  assert.match(String(r.why), /not be smaller|of the database/);
  assert.equal((db.raw.prepare("SELECT type FROM sqlite_master WHERE name = 'participants'").get() as { type: string }).type, 'table');
  assert.deepEqual(read(db), before);
  db.close();
});
