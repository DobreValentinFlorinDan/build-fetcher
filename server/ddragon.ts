import fs from 'node:fs/promises';
import path from 'node:path';
import { config } from './config.ts';

const CDN = 'https://ddragon.leagueoflegends.com';

export interface ItemMeta {
  id: number;
  name: string;
  tags: string[];
  into: number[];
  from: number[];
  specialRecipe: number | null;   // item it transforms from without a recipe (Manamune -> Muramana)
  depth: number;
  goldTotal: number;
  purchasable: boolean;
  onSummonersRift: boolean;
  inStore: boolean;
  passives: string[];   // <passive>Name</passive> blocks parsed from the description
  armor: number;        // stats.FlatArmorMod (0 when the item gives none)
  magicResist: number;  // stats.FlatSpellBlockMod
  antiHeal: boolean;    // applies Grievous Wounds
}

export interface ChampionMeta {
  id: number;
  key: string;          // "Irelia" (image / route key)
  name: string;
  tags: string[];
  info: { attack: number; defense: number; magic: number; difficulty: number };
  attackRange: number;  // stats.attackrange (0 when unknown); ranged champions have more than 300
}

export interface RuneTree {
  id: number;
  key: string;
  name: string;
  icon: string;
  slots: { runes: { id: number; key: string; name: string; icon: string; shortDesc: string }[] }[];
}

export interface StaticData {
  version: string;        // "16.19.1"
  patch: string;          // "16.19"
  previousPatch: string;  // "16.18"
  items: Map<number, ItemMeta>;
  champions: Map<number, ChampionMeta>;
  championsByKey: Map<string, ChampionMeta>;
  runes: RuneTree[];
  runeIds: Set<number>;
  summoners: Map<number, { name: string; image: string }>;
  spells: { key: string; name: string; image: string }[];  // Irelia Q W E R
  /** What each item and Irelia look like in this version's files, to tell what a patch changed (model/patchDiff.ts). */
  fingerprints: Fingerprints;
  /** The previous patch's fingerprints, when its files could be read (else the previous patch counts as changed). */
  previous: (Fingerprints & { version: string }) | null;
  /**
   * Fingerprints of the last few patches by patch ("16.17"), for patch memory (model/weights.ts): what changed between
   * an older game's patch and today's. Loaded by the model's threads (loadPatchHistory); a missing patch is unknown.
   */
  history?: Map<string, Fingerprints>;
  /** Data Dragon's version list (newest first) this was loaded from: workers load the same version from the cache. */
  versions?: string[];
}

export interface Fingerprints { items: Map<number, string>; champion: string }

/** "16.19.1" or "16.19.712.4471" -> "16.19" */
export function toPatch(version: string): string {
  const [major, minor] = version.split('.');
  return `${major}.${minor}`;
}

export function comparePatch(a: string, b: string): number {
  const [a1, a2] = a.split('.').map(Number);
  const [b1, b2] = b.split('.').map(Number);
  return a1 - b1 || a2 - b2;
}

async function fetchJson<T>(url: string): Promise<T> {
  // A stalled connection fails after 30 s (the cache, or the previous patch's absence, takes over) instead of hanging.
  const res = await fetch(url, { signal: AbortSignal.timeout(30_000) });
  if (!res.ok) throw new Error(`Data Dragon ${res.status} for ${url}`);
  return res.json() as Promise<T>;
}

async function cached<T>(version: string, file: string, url: string): Promise<T> {
  const target = path.join(config.ddragonCacheDir, version, file);
  try {
    return JSON.parse(await fs.readFile(target, 'utf8')) as T;
  } catch {
    const data = await fetchJson<T>(url);
    await fs.mkdir(path.dirname(target), { recursive: true });
    await fs.writeFile(target, JSON.stringify(data));
    return data;
  }
}

// --- raw Data Dragon shapes (only the fields we use) ---
interface RawItem {
  name: string; description: string; tags?: string[]; into?: string[]; from?: string[]; specialRecipe?: number; depth?: number;
  inStore?: boolean; maps: Record<string, boolean>; gold: { total: number; purchasable: boolean }; stats?: Record<string, number>;
}
interface RawChampion { key: string; id: string; name: string; tags: string[]; info: ChampionMeta['info']; stats?: { attackrange?: number } }
interface RawSpell {
  id: string; name: string; image: { full: string };
  tooltip?: string; cooldown?: number[]; cost?: number[]; effect?: unknown; datavalues?: unknown; range?: number[];
}

/** The Data Dragon files the app reads for one version, in their published shapes (trimmed copies work too). */
export interface RawStaticData {
  item: { data: Record<string, RawItem> };
  champion: { data: Record<string, RawChampion> };
  runesReforged: RuneTree[];
  summoner: { data: Record<string, { key: string; name: string; image: { full: string } }> };
  /** champion/<championKey>.json: Irelia's spells (and, for the patch diff, her stats and passive) */
  champ: { data: Record<string, { spells: RawSpell[]; stats?: Record<string, number>; passive?: { description?: string } }> };
}

/** The files the patch diff compares between two versions. */
export type RawFingerprintData = Pick<RawStaticData, 'item' | 'champ'>;

/** Items and Irelia as comparable strings: gold, stats and text for items; stats, spell values and texts for Irelia. */
export function fingerprints(raw: RawFingerprintData): Fingerprints {
  const items = new Map<number, string>();
  for (const [id, it] of Object.entries(raw.item.data)) items.set(Number(id), JSON.stringify([it.gold.total, it.stats ?? {}, it.description]));
  const c = raw.champ.data[config.championKey];
  const champion = JSON.stringify({
    stats: c?.stats ?? {},
    spells: (c?.spells ?? []).map((s) => [s.tooltip, s.cooldown, s.cost, s.effect, s.datavalues, s.range]),
    passive: c?.passive?.description ?? '',
  });
  return { items, champion };
}

/** The newest version of each of the `count` newest patches in `versions` (newest first), by patch. */
export function historyVersions(versions: string[], count: number): Map<string, string> {
  const newest = new Map<string, string>();
  for (const v of versions) {
    const p = toPatch(v);
    if (!newest.has(p)) newest.set(p, v);
    if (newest.size >= count) break;
  }
  return newest;
}

/**
 * Fingerprints of the newest version of each of the `count` newest patches in `versions` (newest first), from their
 * Data Dragon item and Irelia files (cached on disk, fetched once). A patch whose files can't be had (offline) is left
 * out: patch memory then treats it as unknown.
 */
export async function loadPatchHistory(versions: string[], count: number): Promise<Map<string, Fingerprints>> {
  const newest = historyVersions(versions, count);
  const out = new Map<string, Fingerprints>();
  await Promise.all([...newest].map(async ([patch, version]) => {
    const base = `${CDN}/cdn/${version}/data/en_US`;
    try {
      const [item, champ] = await Promise.all([
        cached<RawStaticData['item']>(version, 'item.json', `${base}/item.json`),
        cached<RawStaticData['champ']>(version, `${config.championKey}.json`, `${base}/champion/${config.championKey}.json`),
      ]);
      out.set(patch, fingerprints({ item, champ }));
    } catch { /* offline, or not published: unknown */ }
  }));
  return out;
}

/** The newest Data Dragon version and its files (cached on disk). `known` skips asking for the version list again. */
export async function loadStaticData(known?: string[]): Promise<StaticData> {
  const versions = known?.length ? known : await fetchJson<string[]>(`${CDN}/api/versions.json`).catch(async () => {
    // Offline: fall back to the newest cached version
    const dirs = await fs.readdir(config.ddragonCacheDir).catch(() => [] as string[]);
    if (!dirs.length) throw new Error('Data Dragon unreachable and no cache present');
    return dirs.sort((a, b) => comparePatch(b, a));
  });
  const version = versions[0];
  const patch = toPatch(version);
  const previousPatch = versions.map(toPatch).find((p) => p !== patch) ?? patch;
  const previousVersion = versions.find((v) => toPatch(v) === previousPatch && previousPatch !== patch);
  const base = `${CDN}/cdn/${version}/data/en_US`;

  const [item, champion, runesReforged, summoner, champ] = await Promise.all([
    cached<RawStaticData['item']>(version, 'item.json', `${base}/item.json`),
    cached<RawStaticData['champion']>(version, 'champion.json', `${base}/champion.json`),
    cached<RawStaticData['runesReforged']>(version, 'runesReforged.json', `${base}/runesReforged.json`),
    cached<RawStaticData['summoner']>(version, 'summoner.json', `${base}/summoner.json`),
    cached<RawStaticData['champ']>(version, `${config.championKey}.json`, `${base}/champion/${config.championKey}.json`),
  ]);
  // The previous patch's items and Irelia, for the draft model's patch weights. Optional: offline without them, the
  // previous patch counts as changed.
  let previous: RawFingerprintData & { version: string } | undefined;
  if (previousVersion) {
    const prevBase = `${CDN}/cdn/${previousVersion}/data/en_US`;
    previous = await Promise.all([
      cached<RawStaticData['item']>(previousVersion, 'item.json', `${prevBase}/item.json`),
      cached<RawStaticData['champ']>(previousVersion, `${config.championKey}.json`, `${prevBase}/champion/${config.championKey}.json`),
    ]).then(([pItem, pChamp]) => ({ version: previousVersion, item: pItem, champ: pChamp })).catch(() => undefined);
  }
  return { ...buildStaticData(version, previousPatch, { item, champion, runesReforged, summoner, champ }, previous), versions: versions.slice(0, 40) };
}

/** StaticData from raw Data Dragon files: the live loader above, and the tests' committed snapshot (server/test/). */
export function buildStaticData(version: string, previousPatch: string, raw: RawStaticData,
  previous?: RawFingerprintData & { version: string }): StaticData {
  const patch = toPatch(version);
  const items = new Map<number, ItemMeta>();
  for (const [id, it] of Object.entries(raw.item.data)) {
    items.set(Number(id), {
      id: Number(id),
      name: it.name,
      tags: it.tags ?? [],
      into: (it.into ?? []).map(Number),
      from: (it.from ?? []).map(Number),
      specialRecipe: it.specialRecipe ? Number(it.specialRecipe) : null,
      depth: it.depth ?? 1,
      goldTotal: it.gold.total,
      purchasable: it.gold.purchasable,
      onSummonersRift: it.maps['11'] === true,
      inStore: it.inStore !== false,
      passives: [...new Set([...it.description.matchAll(/<passive>([^<]+)<\/passive>/g)].map((m) => m[1].trim()))],
      armor: it.stats?.FlatArmorMod ?? 0,
      magicResist: it.stats?.FlatSpellBlockMod ?? 0,
      antiHeal: /<passive>\s*Grievous Wounds\s*<\/passive>/.test(it.description),
    });
  }

  const champions = new Map<number, ChampionMeta>();
  const championsByKey = new Map<string, ChampionMeta>();
  for (const c of Object.values(raw.champion.data)) {
    const meta: ChampionMeta = { id: Number(c.key), key: c.id, name: c.name, tags: c.tags, info: c.info, attackRange: c.stats?.attackrange ?? 0 };
    champions.set(meta.id, meta);
    championsByKey.set(c.id, meta);
    championsByKey.set(c.name, meta);
  }

  const runes = raw.runesReforged;
  const runeIds = new Set<number>();
  for (const tree of runes) {
    runeIds.add(tree.id);
    for (const slot of tree.slots) for (const r of slot.runes) runeIds.add(r.id);
  }

  const summoners = new Map<number, { name: string; image: string }>();
  for (const s of Object.values(raw.summoner.data)) summoners.set(Number(s.key), { name: s.name, image: s.image.full });

  const keys = ['Q', 'W', 'E', 'R'];
  const spells = raw.champ.data[config.championKey].spells.map((s, i) => ({ key: keys[i], name: s.name, image: s.image.full }));

  return {
    version, patch, previousPatch, items, champions, championsByKey, runes, runeIds, summoners, spells,
    fingerprints: fingerprints(raw),
    previous: previous ? { version: previous.version, ...fingerprints(previous) } : null,
  };
}
