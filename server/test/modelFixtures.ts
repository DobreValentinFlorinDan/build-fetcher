// Synthetic Irelia games for the draft-model tests (server/model/model.test.ts): AP or AD enemy teams, chosen items
// and results drawn from a seeded generator, turned into the rows the model reads from the database.
import { captureTimeline, participantRows } from '../capture.ts';
import type { ModelGameRow, ParticipantLite } from '../model/dataset.ts';
import type { ChampionTotals } from '../model/profiles.ts';
import { captureGame } from '../scraper/ingest.ts';
import { IRELIA, makeMatch, makeTimeline } from './fixtures.ts';

/** Deterministic random numbers in [0, 1) (mulberry32). */
export function rng(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Five ranged mages and five ranged marksmen: the two enemy teams differ only in their damage type. */
export const AP_TEAM = [134, 1, 103, 99, 61];
export const AD_TEAM = [51, 22, 110, 222, 21];
/** Irelia's allies in every game. */
export const ALLIES = [IRELIA, 121, 86, 202, 89];

export interface SynthGame {
  id: string;
  pilot: string;
  enemies: number[];
  /** Completed items in order, and boots. */
  items: number[];
  boots?: number;
  win: boolean;
  minutes?: number;
  position?: 'TOP' | 'MIDDLE';
  creation?: number;
  version?: string;
  /** Irelia's summoner spells (default Flash and Teleport). */
  summoners?: [number, number];
  /** The pilot's ladder standing when the game was stored. */
  tier?: string;
  lp?: number;
}

/** Damage to champions per champion: mages deal magic, marksmen physical, everyone else the fixture default. */
function damage(championId: number) {
  if (AP_TEAM.includes(championId)) return { physical: 1000, magic: 25_000 };
  if (AD_TEAM.includes(championId)) return { physical: 22_000, magic: 800 };
  return null;
}

export function synthMatch(s: SynthGame) {
  const champions = [...ALLIES, ...s.enemies];
  const positions = s.position === 'MIDDLE'
    ? ['MIDDLE', 'JUNGLE', 'TOP', 'BOTTOM', 'UTILITY', 'MIDDLE', 'JUNGLE', 'TOP', 'BOTTOM', 'UTILITY']
    : undefined;
  const match = makeMatch({
    id: s.id, champions, positions, winner: s.win ? 100 : 200, minutes: s.minutes ?? 30, creation: s.creation, version: s.version,
    puuids: champions.map((_, i) => (i === 0 ? s.pilot : `${s.id}-p${i + 1}`)),
  });
  for (const p of match.info.participants) {
    const d = damage(p.championId);
    if (d) { p.physicalDamageDealtToChampions = d.physical; p.magicDamageDealtToChampions = d.magic; }
  }
  if (s.summoners) [match.info.participants[0].summoner1Id, match.info.participants[0].summoner2Id] = s.summoners;
  const minuteOf = [10, 16, 22, 26, 28, 29];
  const timeline = makeTimeline(match, {
    items: [
      ...(s.boots ? [[6, 'ITEM_PURCHASED', s.boots] as [number, string, number]] : []),
      ...s.items.map((id, k) => [minuteOf[k], 'ITEM_PURCHASED', id] as [number, string, number]),
    ],
  });
  return { match, timeline };
}

/** What db.modelGames / db.modelParticipants / db.championTotals would return for these games (platform KR). */
export function modelInputs(games: SynthGame[]) {
  const rows: ModelGameRow[] = [];
  const participants: ParticipantLite[] = [];
  const totals = new Map<string, ChampionTotals>();
  for (const s of games) {
    const { match, timeline } = synthMatch(s);
    const me = match.info.participants[0];
    const cap = captureTimeline(timeline);
    const { row } = captureGame(match, timeline, me, 'otp', undefined);
    rows.push({
      ...row, platform: s.id.split('_')[0], remake: 0, leaver: 0, end_result: 'GameComplete', game_version: match.info.gameVersion, timeline: JSON.stringify(cap),
      ...(s.tier ? { pilot_tier: s.tier, pilot_lp: s.lp ?? null } : {}),
    });
    for (const p of participantRows(match)) {
      participants.push({ match_id: p.match_id, participant_id: p.participant_id, champion_id: p.champion_id, team_id: p.team_id, position: p.position });
      const key = `${p.champion_id}:${p.position}`;
      const t = totals.get(key) ?? {
        champion_id: p.champion_id, position: p.position, games: 0, seconds: 0, dmg_physical: 0, dmg_magic: 0, dmg_true: 0,
        dmg_taken: 0, dmg_mitigated: 0, heal: 0, heal_allies: 0, shield_allies: 0, cc_time: 0,
      };
      t.games++; t.seconds += match.info.gameDuration;
      t.dmg_physical += p.dmg_physical; t.dmg_magic += p.dmg_magic; t.dmg_true += p.dmg_true;
      t.dmg_taken += p.dmg_taken; t.dmg_mitigated += p.dmg_mitigated; t.heal += p.heal; t.heal_allies += p.heal_allies;
      t.shield_allies += p.shield_allies; t.cc_time += p.cc_time;
      totals.set(key, t);
    }
  }
  return { rows, participants, totals: [...totals.values()] };
}

/** Top laners with the fixture's default damage (mostly physical): opponents that say nothing about the enemy team. */
export const NEUTRAL_TOPS = [122, 157, 54, 516, 24];

/**
 * `n` top games of `pilots` pilots: half against the AP team, half against the AD team. The first item is Sterak's
 * Gage (3053) or Blade of the Ruined King (3153) at random; Sterak's wins 70 % against AP and 40 % against AD,
 * BotRK the reverse, so both win 55 % overall and only the draft tells them apart. `effect: false` makes both win
 * 55 % everywhere. `randomOpponent` puts a neutral top laner in lane, so the matchup says nothing about the comp.
 */
export function apAdGames(n: number, pilots: number, seed = 7, o: { effect?: boolean; randomOpponent?: boolean } = {}): SynthGame[] {
  const r = rng(seed);
  return Array.from({ length: n }, (_, i) => {
    const ap = i % 2 === 0;
    const sterak = r() < 0.5;
    const p = o.effect === false ? 0.55 : ap === sterak ? 0.7 : 0.4;
    const team = ap ? AP_TEAM : AD_TEAM;
    const enemies = o.randomOpponent ? [NEUTRAL_TOPS[Math.floor(r() * NEUTRAL_TOPS.length)], ...team.slice(1)] : team;
    return {
      id: `KR_${10_000 + i}`, pilot: `pilot${i % pilots}`, enemies,
      items: [sterak ? 3053 : 3153, 3078, 6333], boots: 3047, win: r() < p,
    };
  });
}

/**
 * `n` top games where the pilot's strength decides a summoner spell (0.9.0's scoring against the expected win rate):
 * half the pilots are strong (Challenger, 1,200 LP: 62 % of games won) and take Ignite in 85 % of their games, the
 * others (Master, 50 LP: 42 %) in 15 %; Teleport otherwise. Ignite itself costs 8 points. Scored against the overall
 * win rate Ignite looks better (its games are mostly the strong pilots'); against each game's expected win rate it
 * looks as it is. Items and the draft carry no information.
 */
export function pilotSkillGames(n: number, pilots: number, seed = 5): SynthGame[] {
  const r = rng(seed);
  return Array.from({ length: n }, (_, i) => {
    const pilot = i % pilots;
    const strong = pilot % 2 === 0;
    const ignite = r() < (strong ? 0.85 : 0.15);
    const p = (strong ? 0.62 : 0.42) - (ignite ? 0.08 : 0);
    const team = i % 2 === 0 ? AP_TEAM : AD_TEAM;
    return {
      id: `KR_${20_000 + i}`, pilot: `pilot${pilot}`, enemies: [NEUTRAL_TOPS[Math.floor(r() * NEUTRAL_TOPS.length)], ...team.slice(1)],
      items: [r() < 0.5 ? 3053 : 3153, 3078, 6333], boots: 3047, win: r() < p,
      summoners: [4, ignite ? 14 : 12], tier: strong ? 'CHALLENGER' : 'MASTER', lp: strong ? 1200 : 50,
    };
  });
}
