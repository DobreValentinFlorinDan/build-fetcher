// B4 personal layer, from your own games (source "me"): your item timings against the high-elo median, which options
// work for you (your results shrunk toward the high-elo estimate, so a few games never override it), and a review of
// each game: what the app showed (rec_log), what you bought, how the lane and the game went. Everything stays on this
// machine. docs/BUILD-SOURCING.md §9.4.
import { toLane } from '../../shared/lanes.ts';
import type { GameReview, Lane, PersonalOption, PersonalReport, PersonalTiming } from '../../shared/types.ts';
import type { TimelineCapture } from '../capture.ts';
import type { Db, RecLogRow } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import { laneDiff, type ParticipantLite, type PreparedGame, type ScoredGame } from './dataset.ts';
import { estimateDecision, type Observation } from './estimate.ts';

/** Pseudo-games of the high-elo estimate: your own results weigh as much after this many games. */
export const PERSONAL_K = 30;

const median = (xs: number[]) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};

/** Decisions the personal layer compares, and how to read each from a game. */
const DECISIONS: { key: string; label: string; option: (s: ScoredGame) => [string, number] | null }[] = [
  { key: 'keystone', label: 'Keystone', option: (s) => [String(s.g.perks[0]), s.wpa0] },
  { key: 'core1', label: 'First item', option: (s) => (s.g.items[0] ? [String(s.g.items[0].item), s.itemWpa[0]] : null) },
  { key: 'core2', label: 'Second item', option: (s) => (s.g.items[1] ? [String(s.g.items[1].item), s.itemWpa[1]] : null) },
  { key: 'core3', label: 'Third item', option: (s) => (s.g.items[2] ? [String(s.g.items[2].item), s.itemWpa[2]] : null) },
  { key: 'boots', label: 'Boots', option: (s) => (s.g.boots && s.bootsWpa !== null ? [String(s.g.boots.item), s.bootsWpa] : null) },
  { key: 'maxFirst', label: 'Ability maxed first', option: (s) => (s.g.maxFirst ? [s.g.maxFirst, s.wpa0] : null) },
  { key: 'summoners', label: 'Summoner spells', option: (s) => [s.g.summoners, s.wpa0] },
];

export function personalReport(scored: ScoredGame[], lane: Lane | null = null): PersonalReport {
  const inLane = (g: PreparedGame) => lane === null || g.lane === lane;
  const mine = scored.filter((s) => s.g.source === 'me' && inLane(s.g));
  const others = scored.filter((s) => s.g.source !== 'me' && inLane(s.g));

  // Item timings: the minute each of your first three items was finished against the high-elo median for that item.
  const minutes = (games: ScoredGame[]) => {
    const m = new Map<number, number[]>();
    for (const s of games) {
      for (const c of s.g.items.slice(0, 3)) {
        let list = m.get(c.item);
        if (!list) { list = []; m.set(c.item, list); }
        list.push(c.t / 60_000);
      }
    }
    return m;
  };
  const you = minutes(mine), them = minutes(others);
  const timings: PersonalTiming[] = [...you].map(([itemId, ts]) => ({
    itemId, you: median(ts)!, games: ts.length, highElo: median(them.get(itemId) ?? []), highEloGames: them.get(itemId)?.length ?? 0,
  })).sort((a, b) => b.games - a.games);

  // Options: your mean win-probability-added, shrunk toward the high-elo lane estimate with PERSONAL_K games.
  const options: PersonalOption[] = [];
  for (const d of DECISIONS) {
    const obs: Observation[] = others.flatMap((s) => {
      const o = d.option(s);
      return o ? [{ option: o[0], wpa: o[1], win: s.g.win, w: 1, lane: s.g.lane, opponent: null, pilot: s.g.pilot, z: [] }] : [];
    });
    const lanes = [...new Set(mine.map((s) => s.g.lane))];
    const highElo = new Map<string, number>();
    for (const l of lanes) {
      for (const e of estimateDecision(obs, { lane: l, opponent: null, z: [] }, { h: Infinity, k: 100, minGames: 1, minShare: 0 }).options) {
        highElo.set(`${l}:${e.option}`, e.laneTheta);
      }
    }
    const byOption = new Map<string, { lane: Lane; wpa: number[] }>();
    for (const s of mine) {
      const o = d.option(s);
      if (!o) continue;
      const key = `${s.g.lane}:${o[0]}`;
      const e = byOption.get(key) ?? { lane: s.g.lane, wpa: [] };
      e.wpa.push(o[1]);
      byOption.set(key, e);
    }
    for (const [key, e] of byOption) {
      const n = e.wpa.length;
      const m = e.wpa.reduce((a, b) => a + b, 0) / n;
      const he = highElo.get(key) ?? 0;
      options.push({
        key: d.key, label: d.label, value: key.split(':')[1], lane: e.lane, games: n, yourLift: m, highEloLift: he,
        blended: (n * m + PERSONAL_K * he) / (n + PERSONAL_K),
      });
    }
  }
  options.sort((a, b) => a.key.localeCompare(b.key) || b.games - a.games);
  return { games: mine.length, wins: mine.filter((s) => s.g.win).length, timings, options };
}

// ------------------------------------------------------------------------------------------- rec_log

/** A rec_log row belongs to the game whose window contains it: champ select up to 20 min before the game, to its end. */
export function linkRecommendations(db: Db, now = Date.now()) {
  const recs = db.unlinkedRecommendations(now - 5 * 60_000);
  if (!recs.length) return 0;
  const games = db.ownGames(200);
  let linked = 0;
  for (const r of recs) {
    const g = games
      .filter((x) => r.created_at >= x.created_at - 20 * 60_000 && r.created_at <= x.created_at + x.game_duration * 1000 + 10 * 60_000)
      .sort((a, b) => Math.abs(a.created_at - r.created_at) - Math.abs(b.created_at - r.created_at))[0];
    if (!g) continue;
    const purchases = JSON.parse(g.purchases) as [number, number][];
    db.linkRecommendation(r.id, g.match_id, { keystone: (JSON.parse(g.perks) as number[])[0], purchases: purchases.map(([id]) => id) }, g.win === 1);
    linked++;
  }
  return linked;
}

// ------------------------------------------------------------------------------------------- post-game review

/** Gold and CS differences against the lane opponent at a minute, from the stored timeline (post-game, exact). */
/** One of your games: what the app showed, what you did, how it went, against the high-elo games of that lane. */
export function reviewGame(matchId: string, prepared: PreparedGame[], scored: ScoredGame[], db: Db, data: StaticData,
  participants: ParticipantLite[]): GameReview | null {
  const own = db.ownGames(500).find((g) => g.match_id === matchId);
  const game = prepared.find((g) => g.matchId === matchId);
  if (!own) return null;
  const lane = toLane(own.position);
  const recs: RecLogRow[] = db.recommendationsFor(matchId);
  const highElo = scored.filter((s) => s.g.source !== 'me' && s.g.lane === lane);
  const medianMinute = (item: number, slot: number) => median(highElo.map((s) => s.g.items[slot]).filter((c) => c?.item === item).map((c) => c!.t / 60_000));
  let lane10: GameReview['lane10'] = null, lane15: GameReview['lane15'] = null;
  if (own.timeline) {
    try {
      const cap = JSON.parse(own.timeline) as TimelineCapture;
      const ps = participants.filter((p) => p.match_id === matchId);
      const me = own.participant_id ?? ps.find((p) => p.champion_id === 39)?.participant_id ?? null;
      const myTeam = ps.find((p) => p.participant_id === me)?.team_id;
      const opp = ps.find((p) => p.team_id !== myTeam && p.champion_id === own.opponent_champion_id)?.participant_id ?? null;
      if (me !== null) { lane10 = laneDiff(cap, me, opp, 10); lane15 = laneDiff(cap, me, opp, 15); }
    } catch { /* unreadable timeline */ }
  }
  const shown = recs.map((r) => ({ phase: r.phase, mode: r.mode as GameReview['shown'][number]['mode'], build: JSON.parse(r.shown) as GameReview['shown'][number]['build'] }));
  const items = (game?.items ?? []).map((c, slot) => ({ itemId: c.item, minute: c.t / 60_000, highEloMinute: medianMinute(c.item, slot) }));
  const draftShown = shown.find((s) => s.phase === 'draft')?.build;
  return {
    matchId, created: own.created_at, lane, opponentId: own.opponent_champion_id, win: own.win === 1,
    kda: [own.kills, own.deaths, own.assists], minutes: Math.round(own.game_duration / 60),
    keystone: (JSON.parse(own.perks) as number[])[0], items, boots: game?.boots?.item ?? null,
    shown,
    followed: draftShown ? {
      keystone: draftShown.keystone === (JSON.parse(own.perks) as number[])[0],
      core: (draftShown.core ?? []).filter((id, i) => items[i]?.itemId === id).length,
    } : null,
    lane10, lane15,
    itemName: Object.fromEntries(items.map((i) => [i.itemId, data.items.get(i.itemId)?.name ?? `#${i.itemId}`])),
  };
}
