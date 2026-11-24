import type { AdaptiveSuggestion, LiveGameState, LivePlayer, LiveSummary } from '../../shared/types.ts';
import type { StaticData } from '../ddragon.ts';
import { teamMagicShare, type ChampionProfiles } from '../model/profiles.ts';
import { isPurchasableItem, type ExclusivityIndex } from './exclusivity.ts';

// Live Client Data API (https://127.0.0.1:2999/liveclientdata/allgamedata) - fields we use.
// It only exposes what the player can see in game (scoreboard items, KDA, CS, vision score) - never enemy gold.
export interface LiveApiPlayer {
  championName: string; riotId?: string; riotIdGameName?: string; summonerName?: string;
  team: 'ORDER' | 'CHAOS'; position: string; level: number; isDead?: boolean;
  items: { itemID: number; count?: number; slot?: number }[];
  scores: { kills: number; deaths: number; assists: number; creepScore: number; wardScore?: number };
}

export interface LiveApiEvent {
  EventID: number; EventName: string; EventTime: number;
  KillerName?: string; VictimName?: string; Assisters?: string[]; Recipient?: string;
  DragonType?: string;      // DragonKill: "Fire", ..., "Elder"
  TurretKilled?: string;    // TurretKilled: "Turret_T2_L_03_A" (T1 = ORDER's turret)
  InhibKilled?: string;     // InhibKilled: "Barracks_T2_R1"
}

export interface AllGameData {
  activePlayer: { riotId?: string; riotIdGameName?: string; summonerName?: string; currentGold: number };
  allPlayers: LiveApiPlayer[];
  events?: { Events: LiveApiEvent[] };
  gameData: { gameTime: number; gameMode?: string };
}

/**
 * You and your lane opponent. The tracker (lcu/liveClient.ts), the threat summary and the live re-ranking all rely on
 * this, so they always agree on who you are and who you face. Without `opponentName` the opponent is the enemy in your
 * position; with it, the enemy playing that champion (the app's matchup, server/matchup.ts), and null means none.
 */
export function identify(raw: AllGameData, opponentName?: string | null) {
  const meId = raw.activePlayer.riotId ?? raw.activePlayer.summonerName;
  const me = raw.allPlayers.find((p) => (p.riotId ?? p.summonerName) === meId)
    ?? raw.allPlayers.find((p) => !!raw.activePlayer.riotIdGameName && p.riotIdGameName === raw.activePlayer.riotIdGameName);
  const opponent = !me ? undefined
    : opponentName !== undefined ? raw.allPlayers.find((p) => p.team !== me.team && opponentName !== null && p.championName === opponentName)
    : raw.allPlayers.find((p) => p.team !== me.team && !!p.position && p.position === me.position);
  return { me, opponent };
}

/** Fallback before champion profiles have enough games (model/profiles.ts): a hand-made healer list. */
const HEALERS = new Set(['Aatrox', 'Vladimir', 'Warwick', 'Soraka', 'Yuumi', 'Sylas', 'Fiddlesticks', 'DrMundo', 'Briar',
  'Olaf', 'Swain', 'Illaoi', 'Sona', 'Nami', 'Seraphine', 'Renata', 'Darius', 'Volibear', 'Trundle', 'Gwen', 'Kayn', 'Nasus', 'Yone']);

/** Irelia-relevant counter items per threat, in preference order. Exclusivity decides which are actually offered. */
const COUNTERS: Record<string, { items: number[]; reason: string }> = {
  ap: { items: [3156, 3091, 6665, 3065, 2504], reason: 'Enemy damage is mostly magic' },
  ad: { items: [6333, 3143, 3026, 3742], reason: 'Enemy damage is mostly physical' },
  heal: { items: [3123, 6609, 3033], reason: 'Enemy team relies on healing' },
  tank: { items: [3153, 3071, 3036], reason: 'Enemy front line is tanky' },
  fed: { items: [3026, 6333], reason: 'Lane opponent is ahead - buy survivability' },
};
const AP_BOOTS = 3111;
const AD_BOOTS = 3047;

/**
 * Threats + counter items. The performance block (CS / vision / gold) comes from lcu/liveClient.ts. With champion
 * profiles from enough stored games, the damage mix, healers and tanks come from what each enemy champion does in
 * games; before that, from Data Dragon's attack / magic ratings, its Tank tag and a hand-made healer list.
 */
export function analyseLiveGame(raw: AllGameData, data: StaticData, excl: ExclusivityIndex, profiles?: ChampionProfiles | null,
  opponentName?: string | null): Omit<LiveGameState, 'performance'> {
  const { me: self, opponent } = identify(raw, opponentName);
  const players: LivePlayer[] = raw.allPlayers.map((p) => {
    const champ = data.championsByKey.get(p.championName);
    return {
      championName: p.championName,
      championId: champ?.id ?? null,
      team: p.team,
      position: p.position,
      level: p.level,
      items: p.items.map((i) => i.itemID),
      kills: p.scores.kills, deaths: p.scores.deaths, assists: p.scores.assists, creepScore: p.scores.creepScore,
      isMe: p === self,
    };
  });

  const me = players.find((p) => p.isMe) ?? null;
  const enemies = me ? players.filter((p) => p.team !== me.team) : [];
  const laneOpponent = opponent ? players[raw.allPlayers.indexOf(opponent)] : null;

  let ap = 0, ad = 0, tanks = 0, healers = 0;
  let apShare: number;
  if (profiles?.calibrated) {
    const ps = enemies.filter((e) => e.championId).map((e) => profiles.get(e.championId!));
    apShare = teamMagicShare(ps);
    tanks = ps.filter((p) => profiles.isTank(p)).length;
    healers = ps.filter((p) => profiles.isHealer(p)).length;
  } else {
    for (const e of enemies) {
      const c = e.championId ? data.champions.get(e.championId) : undefined;
      if (!c) continue;
      ap += c.info.magic; ad += c.info.attack;
      if (c.tags[0] === 'Tank') tanks++;
      if (HEALERS.has(c.key)) healers++;
    }
    apShare = ap + ad > 0 ? ap / (ap + ad) : 0.5;
  }

  const threats: string[] = [];
  if (apShare >= 0.55) threats.push('ap');
  if (apShare <= 0.4) threats.push('ad');
  if (healers >= 2) threats.push('heal');
  if (tanks >= 2) threats.push('tank');
  if (me && laneOpponent && laneOpponent.kills - me.kills >= 3) threats.push('fed');

  const owned = me?.items.filter((id) => id > 0) ?? [];
  const suggestions: AdaptiveSuggestion[] = [];
  const offered = new Set<number>();

  const consider = (itemIds: number[], reason: string) => {
    let firstBlocked: AdaptiveSuggestion | null = null;
    for (const id of itemIds) {
      if (!isPurchasableItem(data.items.get(id)) || offered.has(id)) continue;
      const block = excl.blocker([...owned, ...offered], id);
      if (block === 'Duplicate') return;  // already own one
      if (block) { firstBlocked ??= { itemId: id, reason, blockedBy: block }; continue; }
      if (firstBlocked) firstBlocked.replacement = id;
      suggestions.push(firstBlocked ?? { itemId: id, reason });
      offered.add(id);
      return;
    }
    if (firstBlocked) suggestions.push(firstBlocked);
  };

  for (const t of threats) consider(COUNTERS[t].items, COUNTERS[t].reason);
  if (threats.includes('ap')) consider([AP_BOOTS], 'Magic damage + crowd control: tenacity boots');
  else if (threats.includes('ad')) consider([AD_BOOTS], 'Physical damage: armor boots');

  return {
    active: true,
    gameTime: raw.gameData.gameTime,
    myGold: raw.activePlayer.currentGold,
    players,
    laneOpponent,
    threat: { apShare, tanks, healers },
    suggestions,
    summary: me ? liveSummary(players, me, data, profiles) : null,
  };
}

/**
 * Who is ahead, from what the scoreboard shows: item value (Data Dragon prices of the items everyone can see) and kills.
 * A player is "most fed" only when their items are worth a quarter more than their team's average, as for "Next".
 */
export function liveSummary(players: LivePlayer[], me: LivePlayer, data: StaticData, profiles?: ChampionProfiles | null): LiveSummary {
  const value = (p: LivePlayer) => p.items.reduce((s, id) => s + (data.items.get(id)?.goldTotal ?? 0), 0);
  const us = players.filter((p) => p.team === me.team), them = players.filter((p) => p.team !== me.team);
  const sum = (ps: LivePlayer[], f: (p: LivePlayer) => number) => ps.reduce((s, p) => s + f(p), 0);
  const fed = (team: LivePlayer[]) => {
    const top = [...team].sort((a, b) => value(b) - value(a))[0];
    const avg = sum(team, value) / Math.max(1, team.length);
    return top && top.championId !== null && avg > 0 && value(top) >= 1.25 * avg ? top : null;
  };
  const isAp = (id: number) => {
    if (profiles?.calibrated) return profiles.get(id).magicShare > 0.5;
    const c = data.champions.get(id);
    return !!c && c.info.magic > c.info.attack;
  };
  const enemy = fed(them), ally = fed(us);
  return {
    teamItems: sum(us, value) - sum(them, value),
    kills: sum(us, (p) => p.kills) - sum(them, (p) => p.kills),
    fedEnemy: enemy ? { championId: enemy.championId!, items: value(enemy), ap: isAp(enemy.championId!) } : null,
    fedAlly: ally ? { championId: ally.championId!, items: value(ally), isMe: ally.isMe } : null,
  };
}
