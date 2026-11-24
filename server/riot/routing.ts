// Static Riot routing facts. Platform-routed APIs (league-v4, champion-mastery-v4) live on
// <platform>.api.riotgames.com; regional APIs (match-v5) live on the platform's regional cluster.

export const REGION_HOSTS = {
  AMERICAS: 'americas.api.riotgames.com',
  ASIA: 'asia.api.riotgames.com',
  EUROPE: 'europe.api.riotgames.com',
  SEA: 'sea.api.riotgames.com',
} as const;
export type Region = keyof typeof REGION_HOSTS;

export const PLATFORMS = {
  KR: { region: 'ASIA', label: 'Korea' },
  JP1: { region: 'ASIA', label: 'Japan' },
  EUW1: { region: 'EUROPE', label: 'EU West' },
  EUN1: { region: 'EUROPE', label: 'EU Nordic & East' },
  TR1: { region: 'EUROPE', label: 'Turkey' },
  RU: { region: 'EUROPE', label: 'Russia' },
  ME1: { region: 'EUROPE', label: 'Middle East' },
  NA1: { region: 'AMERICAS', label: 'North America' },
  BR1: { region: 'AMERICAS', label: 'Brazil' },
  LA1: { region: 'AMERICAS', label: 'LAN' },
  LA2: { region: 'AMERICAS', label: 'LAS' },
  OC1: { region: 'SEA', label: 'Oceania' },
  SG2: { region: 'SEA', label: 'Singapore' },
  TW2: { region: 'SEA', label: 'Taiwan' },
  VN2: { region: 'SEA', label: 'Vietnam' },
} as const satisfies Record<string, { region: Region; label: string }>;
export type Platform = keyof typeof PLATFORMS;

/** Ranked tiers, highest first. Apex tiers have a single league list; the rest are paged per division. */
export const TIERS = ['CHALLENGER', 'GRANDMASTER', 'MASTER', 'DIAMOND', 'EMERALD', 'PLATINUM', 'GOLD', 'SILVER', 'BRONZE', 'IRON'] as const;
export type Tier = (typeof TIERS)[number];
export const APEX_TIERS: ReadonlySet<Tier> = new Set(['CHALLENGER', 'GRANDMASTER', 'MASTER']);
export const DIVISIONS = ['I', 'II', 'III', 'IV'] as const;
export type Division = (typeof DIVISIONS)[number];

export const isPlatform = (p: string): p is Platform => p in PLATFORMS;
export const isTier = (t: string): t is Tier => (TIERS as readonly string[]).includes(t);

export const platformHost = (p: Platform) => `${p.toLowerCase()}.api.riotgames.com`;
export const regionHost = (p: Platform) => REGION_HOSTS[PLATFORMS[p].region];

/** Accepts legacy lowercase ids ("euw1") stored by older versions. */
export function normalizePlatform(p: string): Platform {
  const up = p.toUpperCase();
  if (!isPlatform(up)) throw new Error(`Unknown platform "${p}". Known: ${Object.keys(PLATFORMS).join(', ')}`);
  return up;
}

/**
 * A platform from what the League client or Riot says about a server: a platform id ("EUW1", "euw1") or the region
 * name the Riot client shows ("EUW", "EUNE", "LAN", "OCE"). Null when unknown (PBE, a server the app has no host for).
 */
const REGION_NAMES: Record<string, Platform> = {
  KR: 'KR', JP: 'JP1', EUW: 'EUW1', EUNE: 'EUN1', TR: 'TR1', RU: 'RU', ME: 'ME1', NA: 'NA1', BR: 'BR1', LAN: 'LA1', LAS: 'LA2',
  OCE: 'OC1', SG: 'SG2', PH: 'SG2', TH: 'SG2', SEA: 'SG2', TW: 'TW2', VN: 'VN2',
};
export function toPlatform(value: unknown): Platform | null {
  if (typeof value !== 'string' || !value.trim()) return null;
  const up = value.trim().toUpperCase();
  return isPlatform(up) ? up : REGION_NAMES[up] ?? null;
}

/** Account-v1 answers on the three big regional hosts; SEA platforms use Asia's. */
export const accountHost = (p: Platform | null) => {
  const region = p ? PLATFORMS[p].region : 'EUROPE';
  return REGION_HOSTS[region === 'SEA' ? 'ASIA' : region];
};

/** Match ids are prefixed with their platform: "KR_7312..." -> KR */
export const platformOfMatch = (matchId: string) => matchId.split('_')[0];
