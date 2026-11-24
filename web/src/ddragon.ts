// Data Dragon image URLs (Riot's static CDN, the only remote host the dashboard loads anything from) and lookups over
// GET /api/static.
import { createContext, useContext } from 'react';
import type { Champion, StaticView } from './types.ts';
import { SHARDS } from './format.ts';

const CDN = 'https://ddragon.leagueoflegends.com/cdn';

export class Dd {
  readonly byId = new Map<number, Champion>();
  constructor(readonly s: StaticView) {
    for (const c of s.champions) this.byId.set(c.id, c);
  }
  get ready() { return this.s.ready && !!this.s.version; }
  champion(id: number): Champion | undefined { return this.byId.get(id); }
  championName(id: number): string { return this.byId.get(id)?.name ?? (id ? `#${id}` : '—'); }
  championIcon(id: number): string | null {
    const c = this.byId.get(id);
    return c && this.s.version ? `${CDN}/${this.s.version}/img/champion/${c.key}.png` : null;
  }
  itemName(id: number): string { return this.s.items[id]?.name ?? `Item ${id}`; }
  itemIcon(id: number): string | null { return this.s.version ? `${CDN}/${this.s.version}/img/item/${id}.png` : null; }
  runeName(id: number): string { return this.s.runes[id]?.name ?? SHARDS[id]?.name ?? `Rune ${id}`; }
  runeIcon(id: number): string | null {
    const icon = this.s.runes[id]?.icon ?? SHARDS[id]?.icon;
    return icon ? `${CDN}/img/${icon}` : null;
  }
  summonerName(id: number): string { return this.s.summoners[id]?.name ?? `Spell ${id}`; }
  summonerIcon(id: number): string | null {
    const s = this.s.summoners[id];
    return s && this.s.version ? `${CDN}/${this.s.version}/img/spell/${s.icon}` : null;
  }
}

export const EMPTY_STATIC: StaticView = { ready: false, version: null, patch: null, champions: [], items: {}, runes: {}, summoners: {} };
export const DdContext = createContext<Dd>(new Dd(EMPTY_STATIC));
export const useDd = () => useContext(DdContext);
