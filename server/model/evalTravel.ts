// An evaluation that travels with a backup (0.21.0). A pool backup (server/transfer.ts) carries the computer's last
// evaluation: its base (every held-out game's outcomes, the settings it chose, which games it measured: model/evalBase.ts)
// and its summary (the verdicts the app follows), and which games the database held at its full evaluation and at its
// last one (a hash of each match ID, with the base's salt). The computer importing the backup takes that evaluation
// as its own when it is valid there:
//
//  - the same evaluation logic, app, Data Dragon and settings (evalBase.ts portableKey: the key without the data folder);
//  - every game it measured is in this pool now (the import just added them), give or take UPDATE_RULES.maxMissing;
//  - this computer has no evaluation of its own covering as many games.
//
// Then the verdicts apply at once, and the next evaluation is a quick update measuring only the games the other
// computer didn't have when it evaluated (this computer's own), unless the usual rules call for a full one (over 20 %
// more games, a new patch, a week old: docs/EVAL-UPDATE.md). The measurements are not redone: a game's outcome was
// measured by models trained on the other computer's pool, exactly as a quick update keeps the outcomes of games
// measured before newer games came in.
import fs from 'node:fs';
import path from 'node:path';
import v8 from 'node:v8';
import type { StaticData } from '../ddragon.ts';
import type { Db } from '../db.ts';
import { evalMark } from '../autoEval.ts';
import { baseFile, currentKey, currentPortableKey, EVAL_LOGIC, readBase, readBaseHeader, UPDATE_RULES, type BaseHeader } from './evalBase.ts';
import { baseHash, formatEval, type EvalBaseData } from './evaluate.ts';
import { evalDir, readEvaluation, type EvalSummary } from './service.ts';

/** Where a backup keeps the evaluation (entries of the ZIP). */
export const EVAL_ENTRIES = {
  header: 'evaluation/base.json', base: 'evaluation/base.bin', summary: 'evaluation/latest.json',
  seenFull: 'evaluation/seen-full.bin', seenLast: 'evaluation/seen-last.bin',
} as const;

/** What a backup's manifest says of the evaluation it carries. */
export interface ExportedEvaluation {
  fullAt: number;
  lastAt: number;
  /** Held-out games it measured (the base's), and how many its full evaluation held out. */
  known: number;
  fullHeldOut: number;
  portable: string;
}

const hashes = (salt: string, ids: string[]) => Float64Array.from(ids, (id) => baseHash(salt, id)).sort();

/**
 * This computer's last evaluation as a backup of the database `db` carries it, or null: none, one cut short, one saved
 * before 0.21.0 (no portable key) or one whose marks have no row (from before 0.8.4).
 */
export function evaluationToExport(db: Db): { info: ExportedEvaluation; entries: [string, Buffer | { file: string }][] } | null {
  const header = readBaseHeader();
  const summary = readEvaluation();
  if (!header?.portable || !summary || summary.evaluatedAt !== header.lastAt) return null;
  if (header.fullMark.row === null || header.lastMark.row === null) return null;
  const base = readBase();
  if (!base || base.header.lastAt !== header.lastAt) return null;
  const idsUpTo = (row: number) => (db.raw.prepare('SELECT match_id FROM irelia_games WHERE rowid <= ?').all(row) as { match_id: string }[]).map((r) => r.match_id);
  const salt = base.data.salt;
  const f64 = (a: Float64Array) => Buffer.from(a.buffer, a.byteOffset, a.byteLength);
  return {
    info: { fullAt: header.fullAt, lastAt: header.lastAt, known: header.known, fullHeldOut: header.fullHeldOut, portable: header.portable },
    entries: [
      [EVAL_ENTRIES.header, Buffer.from(JSON.stringify(header))],
      [EVAL_ENTRIES.base, { file: baseFile() }],
      [EVAL_ENTRIES.summary, Buffer.from(JSON.stringify(summary))],
      [EVAL_ENTRIES.seenFull, f64(hashes(salt, idsUpTo(header.fullMark.row)))],
      [EVAL_ENTRIES.seenLast, f64(hashes(salt, idsUpTo(header.lastMark.row)))],
    ],
  };
}

/** Where an imported backup leaves its evaluation until this computer takes it or turns it down. */
export const incomingDir = () => path.join(evalDir(), 'incoming');
const ORIGIN = 'origin.json';

/**
 * Saves a backup's evaluation entries (EVAL_ENTRIES, each written by `extract` to the file it is given) for
 * adoptIncomingEvaluation, over any left before. Only a complete set is left: written aside, then moved in.
 */
export async function stageIncomingEvaluation(extract: (entry: string, dest: string) => Promise<void>, origin: { machine: string; exportedAt: number }) {
  const dir = incomingDir();
  const tmp = `${dir}.tmp`;
  fs.rmSync(tmp, { recursive: true, force: true });
  fs.mkdirSync(tmp, { recursive: true });
  for (const entry of Object.values(EVAL_ENTRIES)) await extract(entry, path.join(tmp, path.basename(entry)));
  fs.writeFileSync(path.join(tmp, ORIGIN), JSON.stringify(origin));
  fs.rmSync(dir, { recursive: true, force: true });
  fs.renameSync(tmp, dir);
}

/** An imported evaluation is waiting. */
export const incomingEvaluation = () => fs.existsSync(path.join(incomingDir(), ORIGIN));

/** How many of `ids` hash (with `salt`) into the sorted `set`. */
function countIn(set: Float64Array, salt: string, ids: string[]) {
  let n = 0;
  for (const id of ids) {
    const h = baseHash(salt, id);
    let lo = 0, hi = set.length - 1;
    while (lo <= hi) { const mid = (lo + hi) >> 1; if (set[mid] < h) lo = mid + 1; else if (set[mid] > h) hi = mid - 1; else { n++; break; } }
  }
  return n;
}

const readF64 = (file: string) => { const b = fs.readFileSync(file); return new Float64Array(b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength)); };

/**
 * Takes the evaluation an imported backup left (stageIncomingEvaluation) as this computer's own when it is valid here
 * (see above), else turns it down; either way it is gone afterwards. null: none waiting. Call it while no evaluation
 * runs (it writes the base and latest.json an evaluation writes when it ends).
 */
export function adoptIncomingEvaluation(db: Db, data: StaticData): { adopted: boolean; message: string } | null {
  if (!incomingEvaluation()) return null;
  const dir = incomingDir();
  const done = (adopted: boolean, message: string) => { fs.rmSync(dir, { recursive: true, force: true }); return { adopted, message }; };
  try {
    const origin = JSON.parse(fs.readFileSync(path.join(dir, ORIGIN), 'utf8')) as { machine: string; exportedAt: number };
    const header = JSON.parse(fs.readFileSync(path.join(dir, 'base.json'), 'utf8')) as BaseHeader;
    const summary = JSON.parse(fs.readFileSync(path.join(dir, 'latest.json'), 'utf8')) as EvalSummary;
    const base = v8.deserialize(fs.readFileSync(path.join(dir, 'base.bin'))) as EvalBaseData;
    const from = `${origin.machine}'s evaluation`;
    const whole = base?.report?.evaluatedAt === header.lastAt && base.known instanceof Float64Array && base.known.length === header.known
      && summary.evaluatedAt === header.lastAt;
    if (!whole) return done(false, `${from} came incomplete: not used`);
    if (header.logic !== EVAL_LOGIC || !header.portable || header.portable !== currentPortableKey(db, data)) {
      return done(false, `${from} isn't used: the app version, Data Dragon's or the evaluation settings differ from this computer's (update both to the same version)`);
    }
    const own = readBaseHeader();
    if (own && own.fullHeldOut >= header.fullHeldOut) {
      return done(false, `${from} isn't used: this computer's own covers as many games or more (${own.fullHeldOut.toLocaleString('en-US')} against ${header.fullHeldOut.toLocaleString('en-US')})`);
    }
    const ids = db.ireliaMatchIds();
    const salt = base.salt;
    const knownSorted = Float64Array.from(base.known).sort();
    const missing = header.known - countIn(knownSorted, salt, ids);
    if (missing > UPDATE_RULES.maxMissing * header.known) {
      return done(false, `${from} isn't used: ${missing.toLocaleString('en-US')} of the games it measured aren't in this pool`);
    }
    // This pool's games the other computer didn't have at its last and at its full evaluation: what counts as new here.
    const newSinceLast = ids.length - countIn(readF64(path.join(dir, 'seen-last.bin')), salt, ids);
    const newSinceFull = ids.length - countIn(readF64(path.join(dir, 'seen-full.bin')), salt, ids);
    const now = evalMark(db);
    const lastMark = { row: now.row, games: now.games - newSinceLast, patch: summary.mark?.patch ?? header.lastMark.patch };
    const fullMark = { row: now.row, games: now.games - newSinceFull, patch: header.fullMark.patch };
    fs.mkdirSync(evalDir(), { recursive: true });
    const tmp = `${baseFile()}.tmp`;
    fs.copyFileSync(path.join(dir, 'base.bin'), tmp);
    fs.renameSync(tmp, baseFile());
    const nextHeader: BaseHeader = { ...header, key: currentKey(db, data), fullMark, lastMark, importedFrom: origin };
    fs.writeFileSync(path.join(evalDir(), 'base.json'), `${JSON.stringify(nextHeader, null, 2)}\n`);
    const nextSummary: EvalSummary = {
      ...summary, mark: lastMark, full: { evaluatedAt: summary.full?.evaluatedAt ?? header.fullAt, mark: fullMark },
      dataVersion: db.dataVersion(), importedFrom: origin,
    };
    fs.writeFileSync(path.join(evalDir(), 'latest.json'), `${JSON.stringify(nextSummary, null, 2)}\n`);
    // Its report, to read: not one of this computer's evaluations in the history.
    const stamp = new Date(header.lastAt).toISOString().slice(0, 16).replace(/[:T]/g, '-');
    fs.writeFileSync(path.join(evalDir(), `eval-imported-${stamp}.txt`),
      `Evaluation imported from ${origin.machine}'s backup of ${new Date(origin.exportedAt).toISOString().slice(0, 16)}.\n\n${formatEval(base.report)}\n`);
    return done(true, `${from} is used here: its verdicts apply now, and the next evaluation measures only the `
      + `${newSinceLast.toLocaleString('en-US')} games it didn't have`);
  } catch (err) {
    return done(false, `the imported evaluation couldn't be read (${err instanceof Error ? err.message : String(err)}): not used`);
  }
}
