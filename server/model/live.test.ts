// Stage B in game and after it: live re-ranking (B5: the next item from Live Client data, its stability rule, items in
// progress, the time budget at ten times today's data) and the personal layer (B4: timings, options shrunk toward
// high-elo, rec_log linked to your games).
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { openDb } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import type { AllGameData, LiveApiPlayer } from '../engine/adaptive.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { AD_TEAM, ALLIES, AP_TEAM, apAdGames, modelInputs } from '../test/modelFixtures.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { prepareGames, scoreGames, trainState, type PreparedGame } from './dataset.ts';
import { buildDraftModel, DRAFT_DEFAULTS, type DraftModel } from './draft.ts';
import { LiveReranker } from './live.ts';
import { linkRecommendations, PERSONAL_K, personalReport } from './personal.ts';
import { ChampionProfiles } from './profiles.ts';

let data: StaticData;
let excl: ExclusivityIndex;
let built: { m: DraftModel; prepared: PreparedGame[]; profiles: ChampionProfiles };
before(() => {
  data = loadTestStaticData();
  excl = buildExclusivityIndex(data);
  const inputs = modelInputs(apAdGames(800, 40));
  const profiles = new ChampionProfiles(inputs.totals, [], data);
  const prepared = prepareGames(inputs.rows, inputs.participants, data, profiles, { startingItemWindowMs: 60_000 });
  const state = trainState(prepared);
  built = { m: buildDraftModel(scoreGames(prepared, state), state, profiles), prepared, profiles };
});

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-live-'));
const dbs: { close(): void }[] = [];
after(() => { for (const d of dbs) d.close(); fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

/** A live snapshot: you (Irelia, top) with `items`, your allies, and `enemies` (the first is your lane opponent). */
function live(enemies: number[], items: number[] = [1055], minute = 9): AllGameData {
  const seat = (id: number, i: number, team: 'ORDER' | 'CHAOS'): LiveApiPlayer => ({
    championName: data.champions.get(id)!.key, riotId: `${team}${i}#T`, riotIdGameName: `${team}${i}`, team,
    position: ['TOP', 'JUNGLE', 'MIDDLE', 'BOTTOM', 'UTILITY'][i], level: 8,
    items: (team === 'ORDER' && i === 0 ? items : [1055]).map((itemID) => ({ itemID })),
    scores: { kills: 0, deaths: 0, assists: 0, creepScore: 60 },
  });
  const players = [...ALLIES.map((id, i) => seat(id, i, 'ORDER')), ...enemies.map((id, i) => seat(id, i, 'CHAOS'))];
  return { activePlayer: { riotId: players[0].riotId, currentGold: 500 }, allPlayers: players, gameData: { gameTime: minute * 60 } };
}

const rerank = (r: LiveReranker, raw: AllGameData, m = built.m) => r.rerank(m, 'v1', raw, data, excl, () => undefined, DRAFT_DEFAULTS);

test('live: the next item follows the enemy damage type, labelled with what drove it', () => {
  const vsAp = rerank(new LiveReranker(), live(AP_TEAM));
  const vsAd = rerank(new LiveReranker(), live(AD_TEAM));
  assert.equal(vsAp?.itemId, 3053);
  assert.equal(vsAd?.itemId, 3153);
  assert.equal(vsAp?.slot, 1);
  assert.ok(vsAp!.stats.lift > 0);
  assert.match(vsAp!.reason ?? '', /magic damage/);
  // When one enemy's items are worth far more than the others', the reason names them.
  const fedSyndra = live(AP_TEAM);
  fedSyndra.allPlayers[5].items = [6655, 3020, 1056].map((itemID) => ({ itemID }));
  assert.match(rerank(new LiveReranker(), fedSyndra)?.reason ?? '', /^fed Syndra \(AP\)$/);
  // Owning one completed item moves "Next" to the second slot, never offering what you own.
  const second = rerank(new LiveReranker(), live(AP_TEAM, [3053, 1055]));
  assert.equal(second?.slot, 2);
  assert.notEqual(second?.itemId, 3053);
  assert.ok(second!.alternatives.every((a) => a.itemId !== 3053));
});

test('live: "Next" changes only after another item leads for three polls in a row', () => {
  const r = new LiveReranker();
  assert.equal(rerank(r, live(AP_TEAM))?.itemId, 3053);
  assert.equal(rerank(r, live(AD_TEAM))?.itemId, 3053);   // BotRK leads: 1st poll
  assert.equal(rerank(r, live(AD_TEAM))?.itemId, 3053);   // 2nd
  assert.equal(rerank(r, live(AD_TEAM))?.itemId, 3153);   // 3rd: switch
  assert.equal(rerank(r, live(AP_TEAM))?.itemId, 3153);   // a single poll back does not flip it
  assert.equal(rerank(r, live(AD_TEAM))?.itemId, 3153);
});

test('live: components in progress keep their item as "Next"', () => {
  // Against AP, Sterak's leads; holding a Vampiric Scepter (BotRK component) keeps BotRK.
  const next = rerank(new LiveReranker(), live(AP_TEAM, [1055, 1053]));
  assert.equal(next?.itemId, 3153);
  assert.ok(next?.inProgress);
});

test('live: a re-rank takes under 50 ms with ten times today\'s games', () => {
  // ~1,000 games today: 12 copies of the 800 synthetic games, each a different pilot.
  const scored = built.m.scored;
  const many = Array.from({ length: 12 }, (_, k) => scored.map((s) => ({ ...s, g: { ...s.g, matchId: `${s.g.matchId}-${k}`, pilot: `${s.g.pilot}-${k}` } }))).flat();
  const big = buildDraftModel(many, built.m.state, built.profiles);
  const r = new LiveReranker();
  rerank(r, live(AP_TEAM), big);   // first poll: builds the observation cache and searches the reason
  const times: number[] = [];
  for (let i = 0; i < 15; i++) { rerank(r, live(i % 2 ? AP_TEAM : AD_TEAM), big); times.push(r.lastMs); }
  times.sort((a, b) => a - b);
  const medianMs = times[Math.floor(times.length / 2)];
  console.log(`# re-rank over ${many.length} games: median ${medianMs.toFixed(1)} ms, max ${times[times.length - 1].toFixed(1)} ms`);
  assert.ok(medianMs < 50, `median ${medianMs} ms`);
});

// ------------------------------------------------------------------------------------------- B4 personal layer

test('personal: your timings against the high-elo median, and options shrunk toward high-elo', () => {
  const mine = apAdGames(10, 1, 11).map((g, i) => ({ ...g, id: `KR_${90_000 + i}`, pilot: 'ME', items: [3153, 3078, 6333], win: true }));
  const inputs = modelInputs([...apAdGames(400, 40), ...mine]);
  const profiles = new ChampionProfiles(inputs.totals, [], data);
  const prepared = prepareGames(inputs.rows.map((r) => (r.puuid === 'ME' ? { ...r, pilot_source: 'me' as const } : r)), inputs.participants, data, profiles, { startingItemWindowMs: 60_000 });
  const state = trainState(prepared);
  const report = personalReport(scoreGames(prepared, state), 'TOP');
  assert.equal(report.games, 10);
  assert.equal(report.wins, 10);
  const botrk = report.timings.find((t) => t.itemId === 3153)!;
  assert.ok(Math.abs(botrk.you - 10) < 0.05);        // bought at 10:00 in the fixture, like every high-elo game
  assert.equal(botrk.highElo, botrk.you);
  const opt = report.options.find((o) => o.key === 'core1' && o.value === '3153')!;
  assert.equal(opt.games, 10);
  // Ten wins pull the estimate up, but 30 high-elo pseudo-games hold it: blended sits between, nearer high-elo.
  assert.ok(opt.yourLift > opt.blended && opt.blended > opt.highEloLift);
  assert.ok(Math.abs(opt.blended - (10 * opt.yourLift + PERSONAL_K * opt.highEloLift) / (10 + PERSONAL_K)) < 1e-12);
});

test('personal: what was shown is linked to the game it was shown for', () => {
  const db = openDb(path.join(root, 'personal.db'));
  dbs.push(db);
  const created = Date.now() - 3600_000;
  const [g] = modelInputs([{ ...apAdGames(1, 1)[0], id: 'KR_777', pilot: 'ME', creation: created }]).rows;
  db.insertMatch({ id: 'KR_777', platform: 'KR', patch: g.patch, version: g.game_version!, queue: 420, creation: created, duration: g.game_duration, hasIrelia: true });
  db.insertGame({ ...g, pilot_source: 'me' });
  db.logRecommendation({ session: 'draft:1', phase: 'draft', lane: 'TOP', opponentId: 134, mode: 'draft', shown: { keystone: 8010, core: [3053, 3078, 6333], boots: 3047, items: [] } });
  db.raw.prepare('UPDATE rec_log SET created_at = ? WHERE session = ?').run(created - 5 * 60_000, 'draft:1');
  db.logRecommendation({ session: 'draft:2', phase: 'draft', lane: 'TOP', opponentId: 134, mode: 'baseline', shown: { keystone: 8010, core: [], boots: null, items: [] } });
  db.raw.prepare('UPDATE rec_log SET created_at = ? WHERE session = ?').run(created - 3 * 3600_000, 'draft:2');
  assert.equal(linkRecommendations(db), 1);
  const [rec] = db.recommendationsFor('KR_777');
  assert.equal(rec.session, 'draft:1');
  assert.equal(rec.win, g.win);
  assert.equal(JSON.parse(rec.bought!).keystone, 8010);
  assert.equal(db.recommendation('draft:2')!.match_id, null);   // three hours earlier: another game
  assert.equal(linkRecommendations(db), 0);                      // idempotent
});
