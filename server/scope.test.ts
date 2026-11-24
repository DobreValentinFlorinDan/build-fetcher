// Riot routing, the scrape scope and the disk cache.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import zlib from 'node:zlib';
import { CacheStore, DiskCache, legacyFile } from './riot/diskCache.ts';
import { normalizePlatform, platformHost, platformOfMatch, regionHost } from './riot/routing.ts';
import { parseScope } from './scrapeScope.ts';

test('platforms route to their regional match-v5 host', () => {
  assert.equal(regionHost('KR'), 'asia.api.riotgames.com');
  assert.equal(platformHost('KR'), 'kr.api.riotgames.com');
  assert.equal(regionHost('EUW1'), 'europe.api.riotgames.com');
  assert.equal(regionHost('NA1'), 'americas.api.riotgames.com');
  assert.equal(regionHost('OC1'), 'sea.api.riotgames.com');
  assert.equal(normalizePlatform('euw1'), 'EUW1');
  assert.equal(platformOfMatch('KR_8393787736'), 'KR');
  assert.throws(() => normalizePlatform('XX9'));
});

test('scrape scope defaults to KR, EUW, EUNE, NA and JP apex tiers and accepts new tiers without code changes', () => {
  const base = parseScope({});
  assert.deepEqual(base.platforms, ['KR', 'EUW1', 'EUN1', 'NA1', 'JP1']);
  assert.deepEqual(base.tiers, ['CHALLENGER', 'GRANDMASTER', 'MASTER']);
  const wider = parseScope({ platforms: ['kr', 'euw1'], tiers: ['CHALLENGER', 'DIAMOND', 'emerald'] });
  assert.deepEqual(wider.platforms, ['KR', 'EUW1']);
  assert.deepEqual(wider.tiers, ['CHALLENGER', 'DIAMOND', 'EMERALD']);
  assert.throws(() => parseScope({ tiers: ['WOOD'] }), /WOOD/);
  assert.throws(() => parseScope({ platforms: [] }), /at least one/);
});

test('disk cache round-trips and honours TTLs', async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'irelia-cache-'));
  const cache = new DiskCache(dir);
  await cache.set('match', 'KR', 'KR_1', { a: 1 });
  assert.deepEqual(await cache.get('match', 'KR', 'KR_1'), { a: 1 });
  assert.equal(await cache.get('match', 'KR', 'KR_2'), undefined);

  // Stored an hour ago: older than the 10 min match-id TTL.
  const store = new CacheStore(dir);
  store.write('matchIds', 'KR', 'p1', Buffer.from('["KR_1"]'), Date.now() - 60 * 60_000);
  assert.equal(await cache.get('matchIds', 'KR', 'p1'), undefined);
  assert.equal(cache.hits, 1);

  // A file from before 0.14.0 is read until it is converted, its mtime the TTL's clock.
  const file = legacyFile(dir, 'league', 'KR', 'CHALLENGER');
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(file, zlib.gzipSync(JSON.stringify({ entries: [1] })));
  const later = new DiskCache(dir);
  assert.deepEqual(await later.get('league', 'KR', 'CHALLENGER'), { entries: [1] });
  const old = new Date(Date.now() - 7 * 3600_000);
  await fs.utimes(file, old, old);   // past the 6 h league TTL
  assert.equal(await later.get('league', 'KR', 'CHALLENGER'), undefined);
  // Fetched again: the database has it, and the old file goes.
  await later.set('league', 'KR', 'CHALLENGER', { entries: [2] });
  assert.deepEqual(await later.get('league', 'KR', 'CHALLENGER'), { entries: [2] });
  assert.equal(await fs.stat(file).catch(() => null), null);
  // Compressed with a dictionary once a match and a timeline are both stored.
  const dicts = () => (store.db.prepare('SELECT COUNT(*) AS n FROM codec_dicts').get() as { n: number }).n;
  assert.equal(dicts(), 0);
  await cache.set('timeline', 'KR', 'KR_1', { info: { frames: [] } });
  assert.equal(dicts(), 1);
  assert.deepEqual(await cache.get('timeline', 'KR', 'KR_1'), { info: { frames: [] } });
  store.close(); cache.close(); later.close();
  await fs.rm(dir, { recursive: true, force: true });
});
