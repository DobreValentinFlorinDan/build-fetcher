// npm run eval:builds [-- <data folder>] [--quick] [--threads N] [--auto | --update]
// Offline evaluation of the draft-aware model against the lane baseline (server/model/evaluate.ts): grouped
// cross-validation by pilot, no Riot API calls (Data Dragon comes from the data folder's cache, or its CDN). Opening the
// database upgrades an older one exactly as the app does; nothing else is written to the data folder. The report is
// printed and saved under %APPDATA%\irelia-companion\evals, where the app reads the gate result: a passed gate makes the
// draft-adjusted build the dashboard's default. --quick tries one bandwidth and prior and skips the live replay.
// --threads N: on N threads (the same report); by default every core the available memory holds (model/evalTeam.ts).
// A full evaluation unless given --auto (what the app's Evaluate builds runs: a quick update while the last full
// evaluation still describes the stored games, else a full one; nothing when no games came in since the last one) or
// --update (a quick update, or a full evaluation when there is no full one to add to). 0.16.0, docs/EVAL-UPDATE.md.
// The app runs the same evaluation from the dashboard (Evaluate builds, Full evaluation).
//
// Grouped drafts (0.20.0, docs/EVAL-SCALING.md), a trial the app doesn't use yet:
//   --grouped [N]          a full evaluation with the drafts in N groups (default: about 40 games each), saved as
//                          eval-grouped-…; the app's verdicts and its next quick update stay the last exact evaluation's.
//   --compare-grouped [N]  a full exact evaluation (saved as usual), then the grouped trial on the same games, and the two
//                          side by side (time, every verdict, the lifts), saved as eval-compare-….txt.
// Every report ends with the time each step took.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const args = process.argv.slice(2);
const quick = args.includes('--quick');
const threadsAt = args.indexOf('--threads');
const threads = threadsAt >= 0 ? Number(args[threadsAt + 1]) : undefined;
if (threads !== undefined && !(Number.isInteger(threads) && threads >= 1)) { console.error('--threads takes a whole number, 1 or more'); process.exit(1); }
const auto = args.includes('--auto'), update = args.includes('--update');
if (auto && update) { console.error('--auto or --update, not both'); process.exit(1); }
if (quick && (auto || update)) { console.error('--quick is a full evaluation with one bandwidth and prior: leave out --auto and --update'); process.exit(1); }
const asked = auto ? 'auto' : update ? 'update' : 'full';
const groupedAt = args.findIndex((a) => a === '--grouped' || a === '--compare-grouped');
const compare = args.includes('--compare-grouped');
const groupsArg = groupedAt >= 0 && /^\d+$/.test(args[groupedAt + 1] ?? '') ? Number(args[groupedAt + 1]) : null;
const grouping = groupedAt >= 0 ? groupsArg ?? 0 : null;
if (grouping !== null && (quick || auto || update)) { console.error('--grouped and --compare-grouped are full evaluations: leave out --quick, --auto and --update'); process.exit(1); }
if (args.includes('--grouped') && compare) { console.error('--grouped or --compare-grouped, not both'); process.exit(1); }
const folder = args.find((a, i) => !a.startsWith('--') && !(threadsAt >= 0 && i === threadsAt + 1) && !(groupsArg !== null && i === groupedAt + 1));

function defaultDataDir() {
  const appDir = process.env.IRELIA_CONFIG_DIR ?? path.join(process.env.APPDATA ?? path.join(os.homedir(), '.config'), 'irelia-companion');
  try {
    const dataDir = (JSON.parse(fs.readFileSync(path.join(appDir, 'config.json'), 'utf8')) as { dataDir?: string }).dataDir;
    if (dataDir && fs.existsSync(path.join(dataDir, 'irelia.db'))) return dataDir;
  } catch { /* no installed app settings */ }
  if (fs.existsSync(path.join(appDir, 'data', 'irelia.db'))) return path.join(appDir, 'data');
  return path.resolve('data');
}

// The data folder must be known before server/config.ts loads.
process.env.IRELIA_DATA_DIR = path.resolve(folder ?? process.env.IRELIA_DATA_DIR ?? defaultDataDir());
const { config } = await import('../server/config.ts');
const { openDb } = await import('../server/db.ts');
const { loadStaticData } = await import('../server/ddragon.ts');
const { planEvaluation } = await import('../server/model/evalBase.ts');
const { evaluateDataFolder } = await import('../server/model/runEval.ts');
const { compareReports } = await import('../server/model/evalCompare.ts');
const { evalDir } = await import('../server/model/service.ts');

if (!fs.existsSync(config.dbPath)) { console.error(`No database at ${config.dbPath}`); process.exit(1); }
const db = openDb(config.dbPath);
const data = await loadStaticData();
// --auto with no new games since the last evaluation: nothing to add (as the app's Evaluate builds).
if (asked === 'auto') {
  const plan = planEvaluation(db, data);
  if (plan.path === 'none') {
    db.close();
    console.error(`Nothing to evaluate (${plan.reason}). Without --auto it runs a full evaluation.`);
    process.exit(0);
  }
}
console.error(`Evaluating ${config.dataDir} (no Riot API calls)…`);
const t0 = Date.now();
const log = (line: string) => console.error(`  ${line}`);
if (grouping === null || compare) {
  const exact = await evaluateDataFolder(db, data, { quick, threads, path: asked, log });
  console.log(exact.text);
  console.error(`\n(${Math.round((Date.now() - t0) / 1000)} s) ${exact.path === 'update' ? 'Quick update' : 'Full evaluation'}: ${exact.reason}. Saved as ${exact.file}`);
  if (compare) {
    console.error('\nNow the same games with grouped drafts…');
    const grouped = await evaluateDataFolder(db, data, { threads, grouping, log });
    console.log(`\n${grouped.text}`);
    const text = compareReports({ report: exact.report, ms: exact.ms }, { report: grouped.report, ms: grouped.ms });
    const file = path.join(evalDir(), `eval-compare-${new Date().toISOString().slice(0, 16).replace(/[:T]/g, '-')}.txt`);
    fs.writeFileSync(file, `${text}\n`);
    console.log(`\n${text}`);
    console.error(`\nGrouped trial saved as ${grouped.file}; the comparison as ${file}`);
  }
} else {
  const grouped = await evaluateDataFolder(db, data, { threads, grouping, log });
  console.log(grouped.text);
  console.error(`\n(${Math.round((Date.now() - t0) / 1000)} s) Grouped drafts trial. Saved as ${grouped.file}`);
}
db.close();
