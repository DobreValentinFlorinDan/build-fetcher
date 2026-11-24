import type { Lane } from '../../shared/types.ts';
import type { AllGameData, LiveApiEvent, LiveApiPlayer } from '../engine/adaptive.ts';

// Synthetic Live Client Data for POST /api/dev/simulate-live: exercises the live tracker, the dashboard's live panel and
// the in-game overlay without League running. Irelia (you) vs Darius top, or vs Syndra mid with {lane: "MIDDLE"};
// one game minute per 6 real seconds. {positions: false} leaves every position empty, as customs and the practice tool
// do: the opponent then comes from champ select or the database (server/matchup.ts).

type Seat = [name: string, championName: string, team: 'ORDER' | 'CHAOS', position: string];
const ROSTER: Record<Lane, Seat[]> = {
  TOP: [
    ['You', 'Irelia', 'ORDER', 'TOP'], ['Ally Jungle', 'LeeSin', 'ORDER', 'JUNGLE'], ['Ally Mid', 'Ahri', 'ORDER', 'MIDDLE'],
    ['Ally Bot', 'Jinx', 'ORDER', 'BOTTOM'], ['Ally Support', 'Thresh', 'ORDER', 'UTILITY'],
    ['Enemy Top', 'Darius', 'CHAOS', 'TOP'], ['Enemy Jungle', 'Vi', 'CHAOS', 'JUNGLE'], ['Enemy Mid', 'Syndra', 'CHAOS', 'MIDDLE'],
    ['Enemy Bot', 'Kaisa', 'CHAOS', 'BOTTOM'], ['Enemy Support', 'Nautilus', 'CHAOS', 'UTILITY'],
  ],
  MIDDLE: [
    ['You', 'Irelia', 'ORDER', 'MIDDLE'], ['Ally Top', 'Ornn', 'ORDER', 'TOP'], ['Ally Jungle', 'LeeSin', 'ORDER', 'JUNGLE'],
    ['Ally Bot', 'Jinx', 'ORDER', 'BOTTOM'], ['Ally Support', 'Thresh', 'ORDER', 'UTILITY'],
    ['Enemy Top', 'Darius', 'CHAOS', 'TOP'], ['Enemy Jungle', 'Vi', 'CHAOS', 'JUNGLE'], ['Enemy Mid', 'Syndra', 'CHAOS', 'MIDDLE'],
    ['Enemy Bot', 'Kaisa', 'CHAOS', 'BOTTOM'], ['Enemy Support', 'Nautilus', 'CHAOS', 'UTILITY'],
  ],
};
const OPPONENT: Record<Lane, string> = { TOP: 'Enemy Top', MIDDLE: 'Enemy Mid' };

// [game second, itemIds] - inventory from that time on. You start with the trinket and 500 gold and buy the start at
// 0:20, so the overlay's "Buy first" can be seen. At 24:00 your Plated Steelcaps (3047) become Armored Advance (3174),
// the free tier-3 upgrade, so the overlay's upgrade rule can be seen without League.
const ME_ITEMS: [number, number[]][] = [
  [0, [3340]], [20, [1055, 2003, 3340]], [300, [1055, 1036, 1001, 2055, 3340]], [540, [3153, 1055, 1001, 2055, 3340]],
  [780, [3153, 3047, 1055, 3044, 3340]], [1080, [3153, 3047, 3078, 3364]], [1440, [3153, 3174, 3078, 3053, 3364]],
];
const OPP_ITEMS: Record<Lane, [number, number[]][]> = {
  TOP: [   // Darius
    [0, [1054, 2003, 3340]], [330, [1054, 3044, 3340]], [600, [1054, 3044, 1001, 1036, 3340]],
    [840, [3071, 1054, 3047, 3340]], [1200, [3071, 3047, 3044, 1036, 3364]], [1500, [3071, 3047, 3053, 3364]],
  ],
  MIDDLE: [   // Syndra
    [0, [1056, 2003, 3340]], [330, [1056, 3802, 3340]], [600, [1056, 3802, 1001, 3340]],
    [840, [6655, 1056, 3020, 3340]], [1200, [6655, 3020, 3089, 3364]], [1500, [6655, 3020, 3089, 3157, 3364]],
  ],
};
const at = (sched: [number, number[]][], t: number) => [...sched].reverse().find(([s]) => t >= s)![1];

export function simulateLive(onTick: (raw: AllGameData) => void,
  { startAt = 0, lane = 'TOP' as Lane, positions = true, gameSecondsPerTick = 10, tickMs = 1000 } = {}) {
  let t = startAt;
  const opponent = OPPONENT[lane];
  const events: LiveApiEvent[] = [{ EventID: 0, EventName: 'GameStart', EventTime: 0 }];
  const kill = (time: number, killer: string, victim: string, assisters: string[] = []) =>
    events.push({ EventID: events.length, EventName: 'ChampionKill', EventTime: time, KillerName: killer, VictimName: victim, Assisters: assisters });

  const tick = () => {
    if (t >= 390 && !events.some((e) => e.EventTime === 390)) { kill(390, 'You', opponent, ['Ally Jungle']); events.push({ EventID: events.length, EventName: 'FirstBlood', EventTime: 390, Recipient: 'You' }); }
    if (t >= 700 && !events.some((e) => e.EventTime === 700)) kill(700, 'Enemy Jungle', 'You', [opponent]);
    if (t >= 1020 && !events.some((e) => e.EventTime === 1020)) kill(1020, 'You', opponent);

    const min = t / 60;
    const scoreOf = (name: string) => ({
      kills: events.filter((e) => e.EventName === 'ChampionKill' && e.KillerName === name).length,
      deaths: events.filter((e) => e.EventName === 'ChampionKill' && e.VictimName === name).length,
      assists: events.filter((e) => e.EventName === 'ChampionKill' && e.Assisters?.includes(name)).length,
    });
    const players: LiveApiPlayer[] = ROSTER[lane].map(([name, championName, team, position]) => {
      const me = name === 'You', opp = name === opponent;
      const csRate = position === 'UTILITY' ? 1.2 : me ? 8.1 : opp ? 7.2 : 6.5;
      const items = me ? at(ME_ITEMS, t) : opp ? at(OPP_ITEMS[lane], t) : [];
      return {
        championName, riotId: `${name}#SIM`, riotIdGameName: name, summonerName: `${name}#SIM`, team, position: positions ? position : '',
        level: Math.min(18, 1 + Math.floor(min / 1.4)),
        items: items.map((itemID, slot) => ({ itemID, count: 1, slot })),
        scores: { ...scoreOf(name), creepScore: Math.max(0, Math.floor((min - 1.5) * csRate)), wardScore: Math.round(min * (me ? 0.75 : 0.6) * 10) / 10 },
      };
    });
    onTick({
      activePlayer: { riotId: 'You#SIM', riotIdGameName: 'You', summonerName: 'You#SIM', currentGold: t < 20 ? 500 : 80 + ((t * 9) % 700) },
      allPlayers: players,
      events: { Events: [...events] },
      gameData: { gameTime: t, gameMode: 'CLASSIC' },
    });
    t += gameSecondsPerTick;
  };

  tick();
  const timer = setInterval(tick, tickMs);
  return () => clearInterval(timer);
}
