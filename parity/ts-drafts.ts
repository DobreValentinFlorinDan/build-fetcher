// Parity harness: lane builds and draft recommendations of 0.22.0 for sampled drafts, as the app's model thread makes
// them (model/service.ts buildFromDb, engine/recommend.ts, model/draft.ts recommendDraft), written as JSON with the
// drafts themselves (the Rust parity_drafts reads them from there).
//
//   npx tsx parity/ts-drafts.ts <data-folder> <out.json> [drafts=200] [--memory]
import fs from 'node:fs';
import path from 'node:path';
import { openDb } from '../server/db.ts';
import { buildExclusivityIndex } from '../server/engine/exclusivity.ts';
import { recommend } from '../server/engine/recommend.ts';
import { recommendDraft } from '../server/model/draft.ts';
import { ESTIMATE_DEFAULTS } from '../server/model/estimate.ts';
import { buildFromDb, modelPatches } from '../server/model/service.ts';
import { WEIGHT_DEFAULTS } from '../server/model/weights.ts';
import { regionPatches } from '../server/patches.ts';
import { DEFAULT_SCOPE } from '../server/scrapeScope.ts';
import { loadTestStaticData } from '../server/test/staticData.ts';

const [dir, out] = process.argv.slice(2);
const count = Number(process.argv[4] && !process.argv[4].startsWith('--') ? process.argv[4] : 200);
const memoryOn = process.argv.includes('--memory');
const db = openDb(path.join(dir, 'irelia.db'), { migrate: false });
const data = loadTestStaticData();
const excl = buildExclusivityIndex(data);
const regions = regionPatches(db.gameVersions(), data);
const regionPatch = (p: string) => regions.get(p);
const memory = memoryOn ? { decay: 0.75, patches: 6 } : null;
const started = Date.now();
const built = buildFromDb(db, data, DEFAULT_SCOPE, 60_000, undefined, memory, null);
const buildMs = Date.now() - started;
const patches = modelPatches(db, data);
const rows = db.gamesForPatches(patches, DEFAULT_SCOPE.buildSources);
const engine = { minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15, previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000 };
const settings = {
  weights: { ...WEIGHT_DEFAULTS, regionWeights: DEFAULT_SCOPE.regionWeights, ownPlatform: 'KR', ownRegionBoost: DEFAULT_SCOPE.ownRegionBoost, patchWeights: {}, hotfixes: [], regions: null, memory },
  estimate: { ...ESTIMATE_DEFAULTS },
};
// Drafts: every n-th stored game's, as champ select would show it at different moments.
const games = built.model.scored.map((s) => s.g);
const step = Math.max(1, Math.floor(games.length / count));
const drafts: { lane: 'TOP' | 'MIDDLE'; opponent: number | null; enemies: number[]; allies: number[] }[] = [];
for (let i = 0; drafts.length < count && i < games.length; i += step) {
  const g = games[i];
  const v = drafts.length % 4;
  drafts.push({
    lane: g.lane, opponent: v === 3 ? null : g.opponent,
    enemies: v === 1 ? g.enemies.slice(0, 3) : v === 2 ? [] : g.enemies,
    allies: v === 2 ? [] : g.allies,
  });
}
const results = drafts.map((q) => {
  const baseline = recommend(rows, data, excl, q.opponent, engine, q.lane, regionPatch);
  const draft = recommendDraft(built.model, q, data, excl, regionPatch, baseline, settings, { detail: true });
  return { q, baseline, draft };
});
fs.writeFileSync(out, JSON.stringify({ drafts, results, buildMs, games: built.prepared.length }, null, 1));
console.error(`[ts] ${results.length} drafts on ${built.prepared.length} games (model built in ${buildMs} ms)`);
db.close();
