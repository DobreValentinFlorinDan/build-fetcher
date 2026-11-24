// Parity harness, step 2 (0.22.0's side): the evaluation of a data folder as runEval.ts measures it, on one thread,
// with the tests' Data Dragon snapshot (or the folder's own cached Data Dragon with --ddragon), written as JSON.
//
//   npx tsx parity/ts-eval.ts <data-folder> <out.json> [--quick] [--ddragon]
import fs from 'node:fs';
import path from 'node:path';
import { openDb } from '../server/db.ts';
import { buildExclusivityIndex } from '../server/engine/exclusivity.ts';
import { evaluate, formatEval, type EvalBaseData, type StepTime } from '../server/model/evaluate.ts';
import { evalOptions, evalReads } from '../server/model/evalBase.ts';
import { ChampionProfiles } from '../server/model/profiles.ts';
import { regionPatches } from '../server/patches.ts';
import { loadTestStaticData } from '../server/test/staticData.ts';

const [dir, out] = process.argv.slice(2);
const quick = process.argv.includes('--quick');
const db = openDb(path.join(dir, 'irelia.db'), { migrate: false });
const data = loadTestStaticData();
const { scope, patches, read, sources } = evalReads(db, data, quick);
const opts = evalOptions(scope, quick);
const regions = regionPatches(db.gameVersions(), data);
const profiles = new ChampionProfiles(db.championTotals(patches), db.olderChampionTotals(patches), data);
let timings: StepTime[] = [];
let next: EvalBaseData | null = null;
const started = Date.now();
const startMem = process.memoryUsage().rss;
let peak = startMem;
const sampler = setInterval(() => { peak = Math.max(peak, process.memoryUsage().rss); }, 200);
const report = await evaluate({
  rows: db.modelGamesIter(read, sources), total: db.modelGameCount(read, sources), timelinesOf: (ids) => db.timelinesOf(ids),
  participants: db.modelParticipants(read, sources), classicPatches: patches, data, excl: buildExclusivityIndex(data), profiles,
  regionPatch: (p) => regions.get(p), recLog: db.recentRecommendations(10_000), startingItemWindowMs: 60_000,
  log: (line) => console.error(`[ts] ${line}`),
  progress: (p) => { peak = Math.max(peak, process.memoryUsage().rss); if (p.part === undefined) console.error(`[ts] step ${p.step}/${p.steps}: ${p.label}`); },
  onTimings: (t) => { timings = t; }, salt: 'parity', onBase: (b) => { next = b; },
}, opts);
clearInterval(sampler);
const ms = Date.now() - started;
fs.writeFileSync(out, JSON.stringify({ report, text: formatEval(report), ms, peakRss: peak, timings, known: next ? Array.from((next as EvalBaseData).known) : [] }, null, 2));
console.error(`[ts] done in ${(ms / 1000).toFixed(1)} s, peak RSS ${(peak / 2 ** 20).toFixed(0)} MB`);
db.close();
