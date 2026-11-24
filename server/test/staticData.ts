// The tests' Data Dragon: a trimmed snapshot committed next to this file (scripts/ddragon-snapshot.mjs writes it),
// so the engine tests run offline and keep their results when a new patch ships.
import fs from 'node:fs';
import { buildStaticData, type RawStaticData, type StaticData } from '../ddragon.ts';

interface Snapshot { version: string; previousPatch: string; raw: RawStaticData }

let cached: StaticData | null = null;

export function loadTestStaticData(): StaticData {
  if (!cached) {
    const snap = JSON.parse(fs.readFileSync(new URL('./ddragon-snapshot.json', import.meta.url), 'utf8')) as Snapshot;
    cached = buildStaticData(snap.version, snap.previousPatch, snap.raw);
  }
  return cached;
}
