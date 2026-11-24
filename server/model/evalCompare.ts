// 0.20.0 (docs/EVAL-SCALING.md): where an evaluation's time goes, and an exact evaluation set beside a grouped-drafts
// trial on the same games (npm run eval:builds -- --compare-grouped), as text for the report and the console.
import type { EvalReport, StepTime } from './evaluate.ts';

const secs = (ms: number) => `${(ms / 1000).toFixed(1)} s`;
const pct = (part: number, whole: number) => `${whole > 0 ? Math.round((part / whole) * 100) : 0} %`;
const pts = (x: number) => `${x >= 0 ? '+' : ''}${(x * 100).toFixed(2)} pts`;

/** A step's kind: its label without its numbers ("Comparing builds on fold 2 of 5 (9 settings)" → "Comparing builds on fold … of … (… settings)"). */
export const stepKind = (label: string) => label.replace(/\d[\d,.]*/g, '…');

/** Time by step: every step, then by kind (folds of one step added up), largest first. */
export function formatTimings(timings: StepTime[]): string {
  const total = timings.reduce((t, s) => t + s.ms, 0);
  const lines = ['Time by step (0.20.0)', '---------------------'];
  for (const s of timings) lines.push(`  ${secs(s.ms).padStart(9)}  ${pct(s.ms, total).padStart(5)}  ${s.label}`);
  lines.push(`  ${secs(total).padStart(9)}         in all`);
  const kinds = new Map<string, { ms: number; n: number }>();
  for (const s of timings) {
    const k = stepKind(s.label);
    const a = kinds.get(k) ?? { ms: 0, n: 0 };
    a.ms += s.ms; a.n++;
    kinds.set(k, a);
  }
  if (kinds.size < timings.length) {
    lines.push('', 'By kind, largest first:');
    for (const [k, a] of [...kinds].sort((x, y) => y[1].ms - x[1].ms)) {
      lines.push(`  ${secs(a.ms).padStart(9)}  ${pct(a.ms, total).padStart(5)}  ${k}${a.n > 1 ? ` (${a.n} steps)` : ''}`);
    }
  }
  return lines.join('\n');
}

/** What a run took: its report and, when measured, its wall time. */
export interface ComparedRun { report: EvalReport; ms: number }

/**
 * An exact evaluation and a grouped-drafts trial of the same games side by side: time, every verdict, the lifts, Brier
 * scores and per-decision numbers, and how often held-out games followed each model's pick (a proxy for how often the
 * two pick the same). What to look at before grouping becomes the default: docs/EVAL-SCALING.md, "Adopting it".
 */
export function compareReports(exact: ComparedRun, grouped: ComparedRun): string {
  const a = exact.report, b = grouped.report;
  const lines = ['Exact evaluation against grouped drafts (0.20.0 trial)', '-----------------------------------------------------'];
  lines.push(`Games: ${a.games.toLocaleString('en-US')} (exact) and ${b.games.toLocaleString('en-US')} (grouped)${a.games !== b.games ? ': NOT THE SAME GAMES, the rest does not compare' : ''}`);
  if (b.grouping) lines.push(`Groups: ${b.grouping.groups.toLocaleString('en-US')}, spread ${b.grouping.spread.toFixed(3)} sd around their centre, made in ${secs(b.grouping.ms)}`);
  lines.push(`Time: ${secs(exact.ms)} exact, ${secs(grouped.ms)} grouped (${exact.ms > 0 ? `${Math.round((1 - grouped.ms / exact.ms) * 100)} % less` : 'n/a'})`, '');
  const verdicts: [string, string | undefined, string | undefined][] = [
    ['Draft model gate', a.gate, b.gate],
    ['Patch memory', a.memory?.gate, b.memory?.gate],
    ['Runes and summoners against the draft', a.scoring?.gate, b.scoring?.gate],
    ['Fine-tuning', a.tuning?.gate, b.tuning?.gate],
    ['Late-game swaps', a.swaps?.gate, b.swaps?.gate],
  ];
  let flips = 0;
  lines.push('Verdicts (exact → grouped):');
  for (const [name, x, y] of verdicts) {
    if (x === undefined && y === undefined) continue;
    const same = x === y;
    if (!same) flips++;
    lines.push(`  ${same ? '  ' : '≠ '}${name}: ${x ?? 'not run'} → ${y ?? 'not run'}`);
  }
  lines.push('');
  lines.push(`Draft model lift: ${pts(a.lift.delta)} (${pts(a.lift.low)} to ${pts(a.lift.high)}) → ${pts(b.lift.delta)} (${pts(b.lift.low)} to ${pts(b.lift.high)}), moved ${pts(b.lift.delta - a.lift.delta)}`);
  lines.push(`Brier, draft model: ${a.brier.draft.toFixed(4)} → ${b.brier.draft.toFixed(4)}; baseline ${a.brier.baseline.toFixed(4)} → ${b.brier.baseline.toFixed(4)}`);
  lines.push(`Chosen settings: h=${a.chosen.h}, k=${a.chosen.k} → h=${b.chosen.h}, k=${b.chosen.k}`, '');
  lines.push('Per decision: lift (exact → grouped), held-out games that followed the draft model\'s pick (exact → grouped)');
  let moved = 0;
  for (const d of a.decisions) {
    const e = b.decisions.find((x) => x.key === d.key);
    if (!e) { lines.push(`  ${d.key}: missing in the grouped run`); continue; }
    moved = Math.max(moved, Math.abs(e.draft - d.draft));
    const follow = d.followedDraft > 0 ? `${d.followedDraft.toLocaleString('en-US')} → ${e.followedDraft.toLocaleString('en-US')} (${pts((e.followedDraft - d.followedDraft) / d.followedDraft).replace(' pts', ' %')})` : 'n/a';
    lines.push(`  ${d.key.padEnd(14)} ${pts(d.draft)} → ${pts(e.draft)}   followed ${follow}`);
  }
  lines.push('');
  lines.push(`Summary: ${flips === 0 ? 'every verdict the same' : `${flips} verdict${flips > 1 ? 's' : ''} differ`}; the largest per-decision move ${pts(moved)}; `
    + `the draft model's lift moved ${pts(b.lift.delta - a.lift.delta)}.`);
  return lines.join('\n');
}
