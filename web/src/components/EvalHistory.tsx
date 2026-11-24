import { useEffect, useMemo, useState } from 'react';
import type { EvalHistoryEntry, EvalHistoryPoint } from '../../../shared/types.ts';
import { api } from '../api.ts';
import { Card, Empty } from './common.tsx';

// Data tab: every saved evaluation over time, one small chart per question it answers (0.9.0). Each run is a dot with
// its 95 % interval: a filled dot when the option was better with 95 % confidence, a hollow ring when the interval
// reaches zero, a triangle when the whole interval is below zero. Shape and label carry the verdict as well as colour
// (green and red are close for colour-blind readers), and the table view has every number. A quick update (0.16.0: the
// last full evaluation with the new games added) has a smaller marker, and a "quick" tag in the table.

type Verdict = EvalHistoryPoint['verdict'];
type Key = 'draft' | 'memory' | 'scoring' | 'swaps';

const COLOR: Record<Verdict, string> = { better: '#2dbf8d', unclear: '#8a7f81', worse: '#ff4d58' };
const LABEL: Record<Verdict, string> = { better: 'Better', unclear: 'No clear difference', worse: 'Worse' };
const ICON: Record<Verdict, string> = { better: '✓', unclear: '–', worse: '▼' };

const QUESTIONS: { key: Key; title: string; what: string; good: string }[] = [
  {
    key: 'draft', title: 'Draft-adjusted builds', what: 'against the lane baseline',
    good: 'Better: the app shows draft-adjusted builds by default.',
  },
  {
    key: 'memory', title: 'Patch memory', what: "against today's and the previous patch alone",
    good: 'Better: with Follow the evaluation on, patch memory turns on.',
  },
  {
    key: 'scoring', title: 'Runes against the draft', what: 'against judging runes and summoners by the overall win rate',
    good: 'Better: with Follow the evaluation on, it turns on.',
  },
  {
    key: 'swaps', title: 'Late-game swaps', what: 'against keeping every item',
    good: 'Better: with Follow the evaluation on, the Items view shows them.',
  },
];

const pts = (x: number) => `${x >= 0 ? '+' : '−'}${Math.abs(x * 100).toFixed(2)}`;
const when = (ms: number) => new Date(ms).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
const day = (ms: number) => new Date(ms).toLocaleDateString([], { month: 'short', day: 'numeric' });

/** "Better in 3 of the last 5 evaluations" and how long the newest verdict has held. */
function summary(points: EvalHistoryPoint[]): string {
  const last = points.slice(-10);
  const better = last.filter((p) => p.verdict === 'better').length;
  let run = 0;
  for (let i = points.length - 1; i >= 0 && points[i].verdict === points[points.length - 1].verdict; i--) run++;
  const latest = LABEL[points[points.length - 1].verdict].toLowerCase();
  const held = run > 1 ? `${latest} in each of the last ${run}` : `${latest} in the last one`;
  return `Better in ${better} of the last ${last.length} evaluation${last.length === 1 ? '' : 's'}; ${held}.`;
}

/** Tick values for a range in points of win probability: at most five, 0.5 / 1 / 2 / 5 / 10 points apart. */
function ticks(lo: number, hi: number): { values: number[]; step: number } {
  const span = (hi - lo) * 100;
  const step = [0.5, 1, 2, 5, 10, 20].find((s) => span / s <= 5) ?? 50;
  const values: number[] = [];
  for (let t = Math.ceil((lo * 100) / step) * step; t <= hi * 100 + 1e-9; t += step) values.push(Math.round(t * 10) / 1000);
  return { values, step };
}
/** A tick's label: whole points when the ticks are, else one decimal. */
const tickLabel = (t: number, step: number) => (t === 0 ? '0' : `${t > 0 ? '+' : '−'}${Math.abs(t * 100).toFixed(step >= 1 ? 0 : 1)}`);

function Marker({ x, y, v, small = false }: { x: number; y: number; v: Verdict; small?: boolean }) {
  // A 2px ring in the card's colour keeps a marker legible where it crosses its interval. A quick update's is smaller
  // (0.16.0): the full evaluations stand out.
  const k = small ? 0.7 : 1;
  if (v === 'worse') return <path d={`M${x - 5.5 * k},${y - 4 * k} L${x + 5.5 * k},${y - 4 * k} L${x},${y + 5.5 * k} Z`} fill={COLOR.worse} stroke="var(--panel)" strokeWidth={2} strokeLinejoin="round" />;
  if (v === 'unclear') return <circle cx={x} cy={y} r={4.5 * k} fill="var(--panel)" stroke={COLOR.unclear} strokeWidth={small ? 1.5 : 2} />;
  return <circle cx={x} cy={y} r={5 * k} fill={COLOR.better} stroke="var(--panel)" strokeWidth={2} />;
}

const W = 520, H = 112, TOP = 10, BOTTOM = 88, LEFT = 40, RIGHT = 44;

/** One question over time: each evaluation's difference with its 95 % interval, against zero. */
function QuestionChart({ q, entries }: { q: (typeof QUESTIONS)[number]; entries: EvalHistoryEntry[] }) {
  const runs = entries.map((e) => ({ e, p: e[q.key] })).filter((r): r is { e: EvalHistoryEntry; p: EvalHistoryPoint } => r.p !== null);
  const [hover, setHover] = useState<number | null>(null);
  if (!runs.length) return null;
  const values = runs.flatMap(({ p }) => [p.delta, p.low, p.high]).filter(Number.isFinite);
  let lo = Math.min(0, ...values), hi = Math.max(0, ...values);
  const pad = Math.max((hi - lo) * 0.08, 0.002);
  lo -= pad; hi += pad;
  const y = (v: number) => TOP + ((hi - v) / (hi - lo)) * (BOTTOM - TOP);
  const axis = ticks(lo, hi);
  const col = (W - LEFT - RIGHT) / runs.length;
  const x = (i: number) => LEFT + col * (i + 0.5);
  const latest = runs[runs.length - 1];
  const h = hover !== null ? runs[hover] : null;
  return (
    <div className="mt-3">
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <div className="text-[13px] font-semibold text-ash-50">{q.title} <span className="text-[11px] font-normal text-ash-600">{q.what}</span></div>
        <span className="pill" style={{ borderColor: `${COLOR[latest.p.verdict]}66` }}>
          <span style={{ color: COLOR[latest.p.verdict] }} aria-hidden>{ICON[latest.p.verdict]}</span> {LABEL[latest.p.verdict]} now
        </span>
      </div>
      <p className="mt-0.5 text-[11px] text-ash-400">{summary(runs.map((r) => r.p))} {q.good}</p>
      <div className="relative mt-1" onPointerLeave={() => setHover(null)}>
        <svg viewBox={`0 0 ${W} ${H}`} className="block w-full" role="img"
          aria-label={`${q.title} ${q.what}, ${runs.length} evaluations. Latest: ${pts(latest.p.delta)} points, ${LABEL[latest.p.verdict]}.`}>
          {axis.values.map((t) => (
            <g key={t}>
              <line x1={LEFT} x2={W - RIGHT} y1={y(t)} y2={y(t)} stroke={t === 0 ? '#6f6466' : '#2e2e36'} strokeWidth={1} />
              <text x={LEFT - 6} y={y(t)} dy="0.32em" textAnchor="end" fontSize={10} fill="#6f6466" style={{ fontVariantNumeric: 'tabular-nums' }}>
                {tickLabel(t, axis.step)}
              </text>
            </g>
          ))}
          {runs.map(({ e, p }, i) => (
            <g key={i} opacity={hover === null || hover === i ? 1 : 0.45}>
              {Number.isFinite(p.low) && Number.isFinite(p.high) && (
                <line x1={x(i)} x2={x(i)} y1={y(p.low)} y2={y(p.high)} stroke={COLOR[p.verdict]} strokeOpacity={0.6} strokeWidth={2} strokeLinecap="round" />
              )}
              <Marker x={x(i)} y={y(p.delta)} v={p.verdict} small={e.update} />
            </g>
          ))}
          {/* The newest value, labelled at its end. */}
          <text x={x(runs.length - 1) + 10} y={y(latest.p.delta)} dy="0.32em" fontSize={11} fontWeight={600} fill="#e3d6d7">{pts(latest.p.delta)}</text>
          <text x={LEFT} y={H - 4} fontSize={10} fill="#6f6466">{day(runs[0].e.at)}</text>
          {runs.length > 1 && <text x={W - RIGHT} y={H - 4} fontSize={10} fill="#6f6466" textAnchor="end">{day(latest.e.at)}</text>}
          <text x={(LEFT + W - RIGHT) / 2} y={H - 4} fontSize={10} fill="#6f6466" textAnchor="middle">points of win probability · each evaluation</text>
          {/* Hit targets: each run's whole column, so the pointer only has to be over it. */}
          {runs.map(({ e, p }, i) => (
            <rect key={`hit-${i}`} x={LEFT + col * i} y={0} width={col} height={BOTTOM + 4} fill="transparent" tabIndex={0}
              aria-label={`${when(e.at)}${e.update ? ', quick update' : ''}: ${pts(p.delta)} points, 95% ${pts(p.low)} to ${pts(p.high)}, ${LABEL[p.verdict]}`}
              onPointerEnter={() => setHover(i)} onFocus={() => setHover(i)} onBlur={() => setHover(null)} style={{ outline: 'none', cursor: 'default' }} />
          ))}
          {hover !== null && <line x1={x(hover)} x2={x(hover)} y1={TOP} y2={BOTTOM} stroke="#6f6466" strokeWidth={1} pointerEvents="none" />}
        </svg>
        {h && (
          <div className="pointer-events-none absolute top-0 z-10 w-56 rounded border border-ink-600 bg-ink-900 px-2.5 py-2 text-[11px] shadow-lg"
            style={{ left: `${Math.min(Math.max((x(hover!) / W) * 100, 22), 78)}%`, transform: 'translate(-50%, -105%)' }}>
            <div className="text-[14px] font-semibold text-ash-50">{pts(h.p.delta)} pts</div>
            {Number.isFinite(h.p.low) && <div className="text-ash-400">95% interval {pts(h.p.low)} to {pts(h.p.high)}</div>}
            <div className="mt-1 flex items-center gap-1.5 text-ash-200">
              <span style={{ color: COLOR[h.p.verdict] }} aria-hidden>{ICON[h.p.verdict]}</span>{LABEL[h.p.verdict]}
            </div>
            <div className="mt-1 text-ash-600">{when(h.e.at)} · {h.e.auto ? 'automatic' : 'yours'}{h.e.update ? ' · quick update' : ''} · {h.e.games.toLocaleString('en-US')} games</div>
            {h.e.update && h.e.fullAt !== undefined && (
              <div className="mt-1 text-ash-600">
                {h.e.newGames !== undefined ? `${h.e.newGames.toLocaleString('en-US')} new games added to` : 'Adds the new games to'} the full evaluation of {day(h.e.fullAt)}
              </div>
            )}
            {h.p.reason && <div className="mt-1 text-ash-600">{h.p.reason}</div>}
          </div>
        )}
      </div>
    </div>
  );
}

const cell = (p: EvalHistoryPoint | null) => (p
  ? `${ICON[p.verdict]} ${pts(p.delta)}${Number.isFinite(p.low) ? ` (${pts(p.low)} to ${pts(p.high)})` : ''}`
  : '—');

/** The evaluation history card: refreshed whenever an evaluation finishes (`refresh` changes). */
export function EvalHistoryCard({ refresh }: { refresh: number | null }) {
  const [entries, setEntries] = useState<EvalHistoryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api.evaluations().then((e) => { setEntries(e); setError(null); }, (e) => setError(e instanceof Error ? e.message : String(e)));
  }, [refresh]);
  const shown = useMemo(() => QUESTIONS.filter((q) => entries?.some((e) => e[q.key] !== null)), [entries]);
  return (
    <Card title="Evaluation history" aside={entries?.length ? <span className="text-[11px] text-ash-600">{entries.length} evaluations since {day(entries[0].at)}</span> : undefined}>
      {error && <div className="error">{error}</div>}
      {entries && !entries.length && <Empty>No evaluation saved yet. Evaluate builds (above) saves one each time it runs.</Empty>}
      {entries && entries.length > 0 && (
        <>
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[11px] text-ash-400" aria-label="Legend">
            {(['better', 'unclear', 'worse'] as Verdict[]).map((v) => (
              <span key={v} className="flex items-center gap-1.5">
                <svg width={14} height={14} viewBox="-7 -7 14 14" aria-hidden><Marker x={0} y={0} v={v} /></svg>
                {v === 'better' ? 'Better with 95% confidence' : LABEL[v]}
              </span>
            ))}
            <span className="flex items-center gap-1.5">
              <svg width={14} height={14} viewBox="0 0 14 14" aria-hidden><line x1={7} x2={7} y1={1} y2={13} stroke="#8a7f81" strokeWidth={2} strokeLinecap="round" /></svg>
              95% interval
            </span>
            {entries.some((e) => e.update) && (
              <span className="flex items-center gap-1.5" title="A quick update keeps the last full evaluation's results and adds the new games' own; every verdict is decided again on all of them.">
                <svg width={14} height={14} viewBox="-7 -7 14 14" aria-hidden><Marker x={0} y={0} v="better" small /></svg>
                Smaller: a quick update
              </span>
            )}
          </div>
          <p className="mt-1 text-[11px] text-ash-600">
            Each evaluation tests an option on games held out from it. Above zero, the games that followed it were won more often (estimates).
          </p>
          {shown.map((q) => <QuestionChart key={q.key} q={q} entries={entries} />)}
          <details className="mt-3">
            <summary className="cursor-pointer text-[11px] text-ash-400">As a table</summary>
            <div className="mt-1 overflow-x-auto">
              <table className="w-full text-left text-[11px]" style={{ fontVariantNumeric: 'tabular-nums' }}>
                <thead className="text-ash-600">
                  <tr><th className="py-1 pr-3 font-normal">When</th><th className="pr-3 font-normal">Games</th>
                    {shown.map((q) => <th key={q.key} className="pr-3 font-normal">{q.title} (pts)</th>)}</tr>
                </thead>
                <tbody className="text-ash-200">
                  {[...entries].reverse().map((e) => (
                    <tr key={e.file} className="border-t border-ink-600">
                      <td className="py-1 pr-3 whitespace-nowrap">
                        {when(e.at)}{e.auto ? ' · auto' : ''}
                        {e.update && (
                          <span className="ml-1.5 rounded border border-ink-600 px-1 text-[10px] text-ash-400"
                            title={`Quick update${e.fullAt !== undefined ? ` of the full evaluation of ${day(e.fullAt)}` : ''}${e.newGames !== undefined ? `: ${e.newGames.toLocaleString('en-US')} new games added` : ''}`}>quick</span>
                        )}
                      </td>
                      <td className="pr-3">{e.games.toLocaleString('en-US')}</td>
                      {shown.map((q) => <td key={q.key} className="pr-3 whitespace-nowrap" title={e[q.key]?.reason}>{cell(e[q.key])}</td>)}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </details>
        </>
      )}
    </Card>
  );
}
