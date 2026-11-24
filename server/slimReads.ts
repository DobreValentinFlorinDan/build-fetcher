// Slim stored data (b), 0.14.0: which fields of a cached match and timeline the app's captures read. The captures are
// run on the response wrapped in a Proxy that records every field they touch; the slim copy keeps exactly those, and
// is kept only when every capture gives the same result from it as from the response (slimEntry).
import { captureTimeline, laneOpponents, leavers, matchQuality, participantRows } from './capture.ts';
import { toPatch } from './ddragon.ts';
import type { MatchDto, ParticipantDto, TimelineDto } from './engine/parseMatch.ts';
import { captureGame } from './scraper/ingest.ts';

const IRELIA = 39;

/** Everything the app reads from a cached match (and its timeline, when given): what ingest and the backfill store. */
export function captureOutputs(match: MatchDto, timeline: TimelineDto | null) {
  const info = match.info;
  const out: Record<string, unknown> = {
    row: { patch: toPatch(info.gameVersion), version: info.gameVersion, queue: info.queueId, creation: info.gameCreation, duration: info.gameDuration },
    quality: matchQuality(match),
    participants: participantRows(match),
  };
  // Every Irelia of the match (yours, a pilot's, or one the harvest may come back for): her game as ingest stores it.
  const irelias = info.participants.filter((p: ParticipantDto) => p.championId === IRELIA);
  out.irelias = irelias.map((p) => ({ puuid: p.puuid, position: p.teamPosition, participantId: p.participantId }));
  if (timeline) {
    const cap = captureTimeline(timeline);
    out.timeline = cap;
    out.leavers = leavers(cap);
    out.games = irelias.map((p) => {
      const g = captureGame(match, timeline, p, 'otp', undefined);
      return { row: g.row, opp: laneOpponents(match, cap, p) };
    });
  }
  return out;
}

type Path = string;
/** `value` wrapped so every field read (by key, or by `in`) is added to `reads`, as a path ("info.participants.3.kills"). */
function recorder<T>(value: T, reads: Set<Path>, at = ''): T {
  if (value === null || typeof value !== 'object') return value;
  const children = new Map<PropertyKey, unknown>();
  return new Proxy(value as object, {
    get(target, key, receiver) {
      const v = Reflect.get(target, key, receiver);
      if (typeof key === 'symbol' || !Object.prototype.hasOwnProperty.call(target, key) || (Array.isArray(target) && key === 'length')) return v;
      const path = at ? `${at}.${key}` : key;
      reads.add(path);
      if (v === null || typeof v !== 'object') return v;
      let c = children.get(key);
      if (c === undefined) { c = recorder(v, reads, path); children.set(key, c); }
      return c;
    },
    has(target, key) {
      if (typeof key !== 'symbol') reads.add(at ? `${at}.${key}` : key);
      return Reflect.has(target, key);
    },
    ownKeys(target) {
      // Iterating an object's keys reads which keys it has: every one of them is kept.
      for (const k of Reflect.ownKeys(target)) if (typeof k !== 'symbol') reads.add(at ? `${at}.${k}` : k);
      return Reflect.ownKeys(target);
    },
  }) as T;
}

/**
 * `value` with only the fields in `reads` (and everything under them that was read). An array keeps every element in
 * its place; an element no capture read stays whole.
 */
function keepRead(value: unknown, reads: Set<Path>, at: string): unknown {
  if (value === null || typeof value !== 'object') return value;
  if (Array.isArray(value)) {
    return value.map((v, i) => {
      const p = at ? `${at}.${i}` : String(i);
      return reads.has(p) ? keepRead(v, reads, p) : v;
    });
  }
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(value)) {
    const p = at ? `${at}.${k}` : k;
    if (reads.has(p)) out[k] = keepRead(v, reads, p);
  }
  return out;
}

/**
 * The slim copies of a cached match and its timeline (or of either alone): only the fields the captures read, and only
 * when every capture comes out the same from them. null: leave them as they are (a capture differed, or failed).
 */
export function slimEntry(match: MatchDto, timeline: TimelineDto | null): { match: MatchDto; timeline: TimelineDto | null } | null {
  try {
    const reads = new Set<Path>();
    const tReads = new Set<Path>();
    const before = JSON.stringify(captureOutputs(recorder(match, reads), timeline ? recorder(timeline, tReads) : null));
    const slim = { match: keepRead(match, reads, '') as MatchDto, timeline: timeline ? keepRead(timeline, tReads, '') as TimelineDto : null };
    const after = JSON.stringify(captureOutputs(slim.match, slim.timeline));
    return after === before && after === JSON.stringify(captureOutputs(match, timeline)) ? slim : null;
  } catch { return null; }
}
