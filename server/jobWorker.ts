// One-shot job thread: npm run eval:builds and the data audit, started by the app's buttons (server/jobs.ts). Each
// reads the whole database and the evaluation computes for seconds to minutes, so they get a thread of their own:
// fetching, the dashboard and the live game carry on meanwhile. Nothing is written to the data folder.
//
// workerData: { dataDir, versions, job: 'evaluate' | 'audit', quick?, auto? (started by the app itself), control? (the busy flag, model/evalTeam.ts),
//               path? ('auto' | 'full', the evaluation's: 0.16.0) }
// out:        { progress: { step, steps, label, part? } } and { threads } (the evaluation's) while it runs, then
//             { ok: true, text, file, gate?, threads?, bytesPerGame?, heapPerGame?, layout?, path?, reason? } | { ok: false, error }
import fs from 'node:fs/promises';
import path from 'node:path';
import { parentPort, workerData } from 'node:worker_threads';
import { appConfigDir, readAppConfig } from './appConfig.ts';
import { heapMeasure, threadMeasure } from './progress.ts';
import { auditData, formatAudit } from './audit.ts';
import { config, setDataDir } from './config.ts';
import { openDb } from './db.ts';
import { loadStaticData } from './ddragon.ts';
import { collectGarbage } from './gc.ts';
import { controlFlags, setBusyFlag } from './model/evalTeam.ts';
import { evaluateDataFolder, THREAD_LAYOUT } from './model/runEval.ts';

type Boot = { dataDir: string; versions: string[]; job: 'evaluate' | 'audit'; quick?: boolean; auto?: boolean; control?: SharedArrayBuffer; path?: 'auto' | 'full'; busy?: boolean };

// A thread (parentPort), or since 0.22.0 a process of its own (server/jobs.ts processJob): its boot comes as the first
// message, and champ select or a game as messages that set its own busy flag (a thread reads the app's).
const asProcess = !parentPort;
// Tests only: a job process that can't start (server/jobs.ts then runs it as a thread).
if (asProcess && process.env.IRELIA_TEST_JOB_PROCESS_FAILS === '1') process.exit(3);
const post = (msg: unknown) => { if (asProcess) process.send?.(msg); else parentPort!.postMessage(msg); };
const boot: Boot = asProcess ? await new Promise<Boot>((resolve) => {
  const first = (msg: unknown) => {
    if ((msg as { type?: string })?.type !== 'boot') return;
    process.off('message', first);
    resolve(msg as Boot);
  };
  process.on('message', first);
}) : workerData as Boot;
if (asProcess) {
  const control = controlFlags();
  if (boot.busy) setBusyFlag(control, true);
  boot.control = control;
  process.on('message', (msg) => { const m = msg as { type?: string; on?: boolean }; if (m?.type === 'busy') setBusyFlag(control, m.on === true); });
  // The app went: so does this.
  process.on('disconnect', () => process.exit(0));
}
setDataDir(boot.dataDir);

/** The read-only data audit (server/audit.ts). The report is saved next to the settings, never in the data folder. */
async function audit() {
  const report = await auditData(config.dataDir, { championId: config.championId, onProgress: (progress) => post({ progress }) });
  const text = formatAudit(report);
  const dir = path.join(appConfigDir, 'audits');
  await fs.mkdir(dir, { recursive: true });
  const file = path.join(dir, `audit-${new Date().toISOString().slice(0, 16).replace(/[:T]/g, '-')}`);
  await fs.writeFile(`${file}.json`, `${JSON.stringify(report, null, 2)}\n`);
  await fs.writeFile(`${file}.txt`, `${text}\n`);
  return { text, file: `${file}.txt` };
}

/**
 * npm run eval:builds (model/runEval.ts): the report and the gate result are saved next to the settings. `path`
 * (0.16.0): 'auto' runs a quick update when the plan says (model/evalBase.ts), 'full' every game again.
 */
async function evaluate() {
  const db = openDb(config.dbPath, { migrate: false });
  try {
    const { text, file, report, threads, bytesPerGame, heapPerGame, layout, path: ran, reason } = await evaluateDataFolder(db, await loadStaticData(boot.versions), {
      path: boot.path ?? 'full',
      // Each step leaves the previous one's models and caches behind: collected before it starts, the peak stays low.
      // How far into a step (`part`, every half second) collects nothing: a full collection there would cost seconds.
      quick: boot.quick, auto: boot.auto, progress: (progress) => {
        if (progress.part === undefined) collectGarbage();
        post({ progress });
      },
      // And before each fold's models where no step starts (0.12.0): the last fold's are garbage by then.
      collect: collectGarbage,
      // On every core the memory allows (0.11.0), from what a thread took last time here; champ select or a game stops them.
      bytesPerGame: threadMeasure(THREAD_LAYOUT) ?? undefined, heapPerGame: heapMeasure(THREAD_LAYOUT) ?? undefined, control: boot.control ?? null,
      onThreads: (threads) => post({ threads }),
      // Its thread plan, in the middleware's log (0.15.1): what the memory allowed, to tell a slow evaluation's cause.
      log: (line) => { if (/^(threads|processes|time):/.test(line)) console.log(`[eval] ${line}`); },
      // Settings → GPU (0.13.0): read as it starts.
      gpu: readAppConfig().useGpu === true,
    });
    return { text, file, gate: report.gate, threads, bytesPerGame, heapPerGame, layout, path: ran, reason };
  } finally { db.close(); }
}

let answer: unknown;
try {
  answer = { ok: true, ...(boot.job === 'audit' ? await audit() : await evaluate()) };
} catch (err) {
  answer = { ok: false, error: err instanceof Error ? err.message : String(err) };
}
// A process ends once its answer is through.
if (asProcess) await new Promise<void>((resolve) => { process.send?.(answer, undefined, {}, () => resolve()) ?? resolve(); }).then(() => process.disconnect?.());
else parentPort!.postMessage(answer);
