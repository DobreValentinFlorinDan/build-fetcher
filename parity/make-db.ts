// Parity harness, step 1: a synthetic data folder written by 0.22.0's own storage code (server/db.ts, capture.ts,
// scraper/ingest.ts), so the TypeScript evaluation and the Rust one read the very same database.
//
//   npx tsx parity/make-db.ts <data-folder> [games=2400] [seed=7]
//
// Games: top and mid, AP and AD enemy teams (server/test/modelFixtures.ts), five patches (the current, the previous and
// three older ones for patch memory), ladder standings and summoner choices that depend on the pilot, your own games,
// and late-game sells (swaps).
import fs from 'node:fs';
import path from 'node:path';
import { CAPTURE_VERSION, participantRows } from '../server/capture.ts';
import { openDb } from '../server/db.ts';
import { captureGame, storeGame } from '../server/scraper/ingest.ts';
import { IRELIA, makeMatch, makeTimeline } from '../server/test/fixtures.ts';
import { AD_TEAM, ALLIES, AP_TEAM, NEUTRAL_TOPS, rng } from '../server/test/modelFixtures.ts';

const dir = process.argv[2];
const total = Number(process.argv[3] ?? 2400);
const seed = Number(process.argv[4] ?? 7);
if (!dir) { console.error('usage: make-db.ts <data-folder> [games] [seed]'); process.exit(2); }
fs.rmSync(dir, { recursive: true, force: true });
fs.mkdirSync(dir, { recursive: true });
const db = openDb(path.join(dir, 'irelia.db'));
const r = rng(seed);
const pick = <T>(xs: T[]) => xs[Math.floor(r() * xs.length)];

// Patches: 16.19 (current, Data Dragon's), 16.18 (previous), 16.17-16.15 (patch memory's).
const PATCHES = [
  { version: '16.19.712.4471', share: 0.5, start: Date.UTC(2026, 8, 24) },
  { version: '16.18.700.1000', share: 0.25, start: Date.UTC(2026, 8, 10) },
  { version: '16.17.690.1000', share: 0.1, start: Date.UTC(2026, 7, 27) },
  { version: '16.16.680.1000', share: 0.08, start: Date.UTC(2026, 7, 13) },
  { version: '16.15.670.1000', share: 0.07, start: Date.UTC(2026, 6, 30) },
];
const PLATFORMS = ['KR', 'KR', 'KR', 'EUW1', 'NA1'];
const TIERS = ['CHALLENGER', 'GRANDMASTER', 'MASTER', 'MASTER', 'DIAMOND', null];
const pilots = Array.from({ length: 90 }, (_, i) => ({
  puuid: `pilot-${i}`, strong: i % 3 === 0, tier: TIERS[i % TIERS.length], lp: (i * 137) % 1400, mastery: 80_000 + ((i * 7919) % 900_000),
  source: i % 7 === 0 ? 'main' : i % 11 === 0 ? 'harvested' : 'otp',
}));
const MID_ENEMIES = [134, 1, 103, 99, 61, 157, 238];

let n = 0;
db.tx(() => {
  for (let i = 0; i < total; i++) {
    const p = pilots[i % pilots.length];
    const me = i % 97 === 0;   // your own games
    const u = r();
    let acc = 0, pi = 0;
    for (; pi < PATCHES.length - 1; pi++) { acc += PATCHES[pi].share; if (u < acc) break; }
    const patch = PATCHES[pi];
    const platform = pick(PLATFORMS);
    const id = `${platform}_${7_000_000_000 + i}`;
    const mid = r() < 0.3;
    const ap = r() < 0.5;
    const team = ap ? AP_TEAM : AD_TEAM;
    const opponent = mid ? pick(MID_ENEMIES) : pick(NEUTRAL_TOPS);
    const enemies = [opponent, ...team.slice(1)];
    const sterak = r() < 0.5;
    const ignite = r() < (p.strong ? 0.8 : 0.2);
    const win = r() < (p.strong ? 0.6 : 0.45) + (ap === sterak ? 0.08 : -0.08) - (ignite ? 0.04 : 0);
    const minutes = 22 + Math.floor(r() * 22);
    const six = minutes >= 36 && r() < 0.7;
    const items = [sterak ? 3053 : 3153, 3078, r() < 0.6 ? 6333 : 3071, ...(six ? [3065, 3026, 3742] : [])];
    const boots = r() < 0.75 ? 3047 : 3111;
    const champions = [...ALLIES, ...enemies];
    const positions = mid
      ? ['MIDDLE', 'JUNGLE', 'TOP', 'BOTTOM', 'UTILITY', 'MIDDLE', 'JUNGLE', 'TOP', 'BOTTOM', 'UTILITY']
      : undefined;
    const match = makeMatch({
      id, champions, positions, winner: win ? 100 : 200, minutes,
      creation: patch.start + Math.floor(r() * 12 * 86_400_000), version: patch.version,
      puuids: champions.map((_, k) => (k === 0 ? (me ? 'ME' : p.puuid) : `${id}-p${k + 1}`)),
    });
    for (const pp of match.info.participants) {
      if (AP_TEAM.includes(pp.championId)) { pp.physicalDamageDealtToChampions = 1000; pp.magicDamageDealtToChampions = 25_000; }
      if (AD_TEAM.includes(pp.championId)) { pp.physicalDamageDealtToChampions = 22_000; pp.magicDamageDealtToChampions = 800; }
    }
    match.info.participants[0].summoner2Id = ignite ? 14 : 12;
    const minuteOf = [10, 16, 22, 26, 28, 29];
    const events: [number, string, number, number?][] = [
      [6, 'ITEM_PURCHASED', boots],
      ...items.map((id2, k) => [minuteOf[k], 'ITEM_PURCHASED', id2] as [number, string, number]),
    ];
    // Late-game swaps: some full inventories sell the boots or the first item for a Sundered Sky.
    if (six && minutes >= 40) {
      const sold = r() < 0.5 ? boots : items[0];
      events.push([33, 'ITEM_SOLD', sold], [34, 'ITEM_PURCHASED', 6610]);
    }
    const skills = r() < 0.7 ? 'QEWQQRQEQERQEEWWRW' : 'QWEQQRQWQWRWWEEERE';
    const timeline = makeTimeline(match, { items: events, skills, ...(i % 53 === 0 ? { leaver: { pid: 7, fromMinute: 12 } } : {}) });
    const source = me ? 'me' : p.source as 'otp' | 'main' | 'harvested';
    const player = me ? undefined : {
      puuid: p.puuid, platform, irelia_mastery: p.mastery, tier: p.tier ?? 'UNKNOWN', league_points: p.lp, is_otp: source === 'otp' ? 1 : 0,
      last_checked: null, last_match_scan: null, pilot_class: source === 'main' ? 'main' as const : 'otp' as const, irelia_points: p.mastery,
      irelia_last_play: null, irelia_rank: 1,
    };
    const game = captureGame(match, timeline, match.info.participants[0], source, p.tier ? player : undefined);
    db.insertMatch({ id, platform, patch: patch.version.split('.').slice(0, 2).join('.'), version: patch.version, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
    db.insertParticipants(id, participantRows(match));
    db.setQuality(id, false, 'GameComplete', CAPTURE_VERSION);
    storeGame(db, game);
    n++;
  }
});
db.setMeta('me_puuid', 'ME');
db.close();
console.log(`${n} games in ${path.join(dir, 'irelia.db')} (Irelia is champion ${IRELIA})`);
