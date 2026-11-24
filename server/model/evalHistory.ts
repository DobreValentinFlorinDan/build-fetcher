// The Data tab's evaluation history (0.9.0): every report Evaluate builds saved (yours and the automatic ones, oldest
// first), each reduced to its comparisons: the draft-adjusted builds against the lane baseline, patch memory against two
// patches, runes against the draft against today's scoring, and late-game swaps against keeping every item (0.17.0).
// Reads only the evals folder. Quick updates (0.16.0, eval-[auto-]update-…) are marked: the last full evaluation's
// measurements with the new games' added.
import fs from 'node:fs';
import path from 'node:path';
import type { EvalHistoryEntry, EvalHistoryPoint } from '../../shared/types.ts';
import type { EvalReport } from './evaluate.ts';

/** How many evaluations the history shows at most (the newest). */
export const HISTORY_LIMIT = 30;

const finite = (x: unknown): x is number => typeof x === 'number' && Number.isFinite(x);

/** better: the gate passed; worse: the whole interval is below zero; else unclear (the interval reaches zero). */
function point(delta: unknown, low: unknown, high: unknown, passed: boolean, reason: string): EvalHistoryPoint | null {
  if (!finite(delta)) return null;
  const lo = finite(low) ? low : NaN, hi = finite(high) ? high : NaN;
  return { delta, low: lo, high: hi, verdict: passed ? 'better' : hi < 0 ? 'worse' : 'unclear', reason };
}

/** One saved report as the history shows it; null when it can't be read as one. */
export function historyEntry(r: Partial<EvalReport>, file: string): EvalHistoryEntry | null {
  if (!finite(r.evaluatedAt) || !r.lift) return null;
  // A grouped-drafts trial (0.20.0) isn't one of the app's evaluations: its numbers are close to them, not the same.
  if (r.grouping || file.startsWith('eval-grouped-')) return null;
  const draft = point(r.lift.delta, r.lift.low, r.lift.high, r.gate === 'passed', r.gateReason ?? '');
  if (!draft) return null;
  const m = r.memory;
  const memory = m && m.grid?.length ? point(m.full?.delta, m.full?.low, m.full?.high, m.gate === 'passed', m.gateReason ?? '') : null;
  const c = r.scoring;
  const scoring = c && c.decisions?.length ? point(c.adjusted?.delta, c.adjusted?.low, c.adjusted?.high, c.gate === 'passed', c.gateReason ?? '') : null;
  // Late-game swaps (0.17.0): missing in reports from before; no point without a situation to compare.
  const s = r.swaps;
  const swaps = s && s.situations > 0 ? point(s.lift?.delta, s.lift?.low, s.lift?.high, s.gate === 'passed', s.gateReason ?? '') : null;
  // A quick update says so in its report, and in its name.
  const u = r.update;
  const update = !!u || /^eval-(?:auto-)?update-/.test(file);
  return {
    at: r.evaluatedAt, auto: file.startsWith('eval-auto-'), update,
    ...(u && finite(u.fullAt) ? { fullAt: u.fullAt } : {}), ...(u && finite(u.newGames) ? { newGames: u.newGames } : {}),
    file, games: r.games ?? 0, draft, memory, scoring, swaps,
  };
}

/** The newest `limit` saved evaluations in `dir`, oldest first. A file that can't be read is skipped. */
export function evalHistory(dir: string, limit = HISTORY_LIMIT): EvalHistoryEntry[] {
  let names: string[];
  try { names = fs.readdirSync(dir).filter((f) => /^eval-(?:auto-)?(?:update-)?[\d-]+\.json$/.test(f)); } catch { return []; }
  const entries: EvalHistoryEntry[] = [];
  for (const name of names) {
    try {
      const e = historyEntry(JSON.parse(fs.readFileSync(path.join(dir, name), 'utf8')) as Partial<EvalReport>, name);
      if (e) entries.push(e);
    } catch { /* a file being written, or not a report */ }
  }
  return entries.sort((a, b) => a.at - b.at).slice(-limit);
}
