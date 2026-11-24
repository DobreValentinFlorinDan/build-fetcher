// One cached game slimmed (Slim stored data's step 4 (b), 0.14.4): its match and timeline read back, slimmed where
// every capture comes out the same, and compressed again, with nothing written. Pure, so the slimming runs on several
// threads (server/slimPartWorker.ts) while one thread writes (server/slim.ts apply); one game takes about 30 ms of a
// core, two thirds of it in the captures' check.
import type { Dictionary } from './codec.ts';
import { pack, unpack } from './codec.ts';
import type { MatchDto, TimelineDto } from './engine/parseMatch.ts';
import { slimEntry } from './slimReads.ts';

export interface PartGame { scope: string; key: string; rows: { kind: string; stored_at: number; data: Uint8Array }[] }
/** What goes back into the cache: the slim blob, or the original (`slim` false) where slimming changed a capture or saved nothing. */
export interface PartPut { kind: string; storedAt: number; blob: Uint8Array; slim: boolean; saved: number }
export interface PartResult { scope: string; key: string; puts: PartPut[] }

/**
 * The cache's dictionaries: every one by id (to read), and the current one (to write with). `pack`: the slim thread's
 * own CacheStore.encode, which makes the cache's first dictionary when there is none yet.
 */
export interface PartCodec { get: (id: number) => Buffer | undefined; current: Dictionary | null; pack?: (kind: string, raw: Buffer) => Buffer }

const parse = <T>(json: string): T | null => { try { return JSON.parse(json) as T; } catch { return null; } };

export function slimGame(g: PartGame, codec: PartCodec): PartResult {
  const orig = new Map(g.rows.map((r) => [r.kind, { storedAt: r.stored_at, data: r.data, json: unpack(r.data, codec.get).toString('utf8') }]));
  const match = orig.get('match'), timeline = orig.get('timeline');
  let slim: ReturnType<typeof slimEntry> = null;
  const matchDto = match ? parse<MatchDto>(match.json) : null;
  // A match with an Irelia and no cached timeline stays whole: a later timeline (the harvest) reads more of it.
  const hasIrelia = matchDto?.info?.participants?.some((p) => p.championId === 39) ?? false;
  if (matchDto && !(hasIrelia && !timeline)) slim = slimEntry(matchDto, timeline ? parse<TimelineDto>(timeline.json) : null);
  const puts: PartPut[] = [];
  for (const [kind, o] of orig) {
    const next = slim ? Buffer.from(JSON.stringify(kind === 'match' ? slim.match : slim.timeline)) : null;
    const blob = next ? (codec.pack ? codec.pack(kind, next) : pack(next, codec.current)) : null;
    // Slim only if it reads back as the slim JSON (and saves something); else the original goes back as it was.
    const ok = !!(next && blob && blob.length < o.data.length && readsBack(blob, next, codec));
    puts.push(ok ? { kind, storedAt: o.storedAt, blob: blob!, slim: true, saved: o.data.length - blob!.length } : { kind, storedAt: o.storedAt, blob: o.data, slim: false, saved: 0 });
  }
  return { scope: g.scope, key: g.key, puts };
}

function readsBack(blob: Uint8Array, raw: Buffer, codec: PartCodec) {
  try { return unpack(blob, codec.get).equals(raw); } catch { return false; }
}
