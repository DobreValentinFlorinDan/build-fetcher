// Slim stored data's thread (0.14.0, server/slim.ts; started by server/slimControl.ts with fetching paused): the
// backup, the baseline, the slimming and the check, the manifest saved after each. The main thread rolls back when the
// check finds a difference (it has to close the database for that).
//   in  workerData: { dataDir, versions, manifest, threads? }
//   out { type: 'progress', step, phase, part, label, count } | { type: 'result', manifest, identical, differences } | { type: 'error', error, manifest }
import { parentPort, workerData } from 'node:worker_threads';
import type { SlimCount, SlimManifest, SlimStep } from '../shared/types.ts';
import { config, setDataDir } from './config.ts';
import { openDb } from './db.ts';
import { loadPatchHistory, loadStaticData } from './ddragon.ts';
import { collectGarbage, exposeGcToThreads } from './gc.ts';
import { HISTORY_PATCHES } from './model/weights.ts';
import { CacheStore } from './riot/diskCache.ts';
import { apply, backup, differences, planSlim, readBaseline, results, saveBaseline, type SlimTick, spaceProblem, writeManifest } from './slim.ts';

const boot = workerData as { dataDir: string; versions: string[]; manifest: SlimManifest; threads?: number };
setDataDir(boot.dataDir);
exposeGcToThreads();
const m = boot.manifest;
const progress = (step: SlimStep, phase: string, part: number | null, label: string, count: SlimCount | null = null) =>
  parentPort!.postMessage({ type: 'progress', step, phase, part, label, count });
/** A step's phases, as its progress (the baseline's and the check's labels after what they are doing). */
const ticks = (step: SlimStep, prefix = ''): SlimTick => (phase, label, part, count) =>
  progress(step, phase, part, prefix ? `${prefix}: ${label}…` : label, count ?? null);

const db = openDb(config.dbPath, { migrate: false, busyTimeoutMs: 30_000 });
/**
 * The check's quick evaluation on this thread alone (0.15.2), the same results as on several: in the app every thread
 * shares one 4 GB heap with the model thread, each more takes about 1.3 GB at 121,000 games, and 0.15.1 ran out of it
 * before slimming. The check's parts' leftovers are collected between them.
 */
const check = { threads: boot.threads ?? 1, collect: () => collectGarbage(true) };
const store = new CacheStore(config.riotCacheDir);
try {
  const loaded = await loadStaticData(boot.versions);
  const data = { ...loaded, history: await loadPatchHistory(loaded.versions ?? boot.versions, HISTORY_PATCHES) };

  // 1-2. Free space, then the backup.
  const plan = planSlim(db, store, data);
  m.plan = plan.plan;
  const problem = await spaceProblem(boot.dataDir, plan.plan.neededBytes);
  if (problem) throw Object.assign(new Error(problem), { refused: true });
  m.step = 'backup';
  writeManifest(boot.dataDir, m, db);
  await backup(boot.dataDir, db, store, plan, m, ticks('backup'));

  // 3. The baseline.
  m.step = 'baseline';
  writeManifest(boot.dataDir, m, db);
  m.baseline = saveBaseline(boot.dataDir, await results(db, data, { ...check, tick: ticks('baseline', 'Recording the results before') }));
  writeManifest(boot.dataDir, m, db);

  // 4. Slimming.
  m.step = 'apply';
  writeManifest(boot.dataDir, m, db);
  await apply(boot.dataDir, db, store, m, ticks('apply'), { threads: boot.threads });
  // The tests' difference: some results change, and the check has to catch it.
  if (process.env.IRELIA_SLIM_TEST_DIFFERENCE === '1') db.raw.exec('UPDATE irelia_games SET win = 1 - win WHERE match_id IN (SELECT match_id FROM irelia_games ORDER BY match_id LIMIT 8)');

  // 5. The check.
  m.step = 'verify';
  writeManifest(boot.dataDir, m, db);
  check.collect();
  const after = await results(db, data, { ...check, tick: ticks('verify', 'Checking the results after') });
  const diff = differences(readBaseline(boot.dataDir), JSON.parse(JSON.stringify(after)));
  if (!diff.length) { m.step = 'verified'; writeManifest(boot.dataDir, m, db); }
  else { m.differences = diff; writeManifest(boot.dataDir, m, db); }
  parentPort!.postMessage({ type: 'result', manifest: m, identical: diff.length === 0, differences: diff });
} catch (err) {
  const e = err as Error & { refused?: boolean };
  parentPort!.postMessage({ type: 'error', error: e.message, refused: e.refused === true, manifest: m });
} finally {
  store.close();
  db.close();
}
