import { useId } from 'react';
import { LANE_LABEL, toLane } from '../../../shared/lanes.ts';
import type { LiveGameState, LivePerformance, PerformanceSample } from '../../../shared/types.ts';
import { img, type Meta } from '../api.ts';

export const clock = (s: number) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
export const signed = (n: number, digits = 0) => `${n > 0 ? '+' : n < 0 ? '−' : '±'}${Math.abs(n).toFixed(digits)}`;
export const goldK = (n: number) => (Math.abs(n) >= 1000 ? `${signed(n / 1000, 1)}k` : signed(n));
export const diffTone = (n: number) => (n > 0 ? 'text-[#2dbf8d]' : n < 0 ? 'text-blood-400' : 'text-ash-200');

/** Gold-difference history as a zero-centred area sparkline (green above, red below). */
export function Sparkline({ points, field = 'gold', className = 'h-10 w-full' }: {
  points: PerformanceSample[]; field?: 'gold' | 'cs' | 'vision'; className?: string;
}) {
  const uid = `spark${useId().replace(/[^\w-]/g, '')}`;
  if (points.length < 2) return <div className={`${className} grid place-items-center text-[10px] text-ash-600`}>collecting…</div>;
  const W = 200, H = 40;
  const vals = points.map((p) => p[field]);
  const span = Math.max(1, ...vals.map(Math.abs));
  const x = (i: number) => (i / (points.length - 1)) * W;
  const y = (v: number) => H / 2 - (v / span) * (H / 2 - 2);
  const line = vals.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(' ');
  const area = `0,${H / 2} ${line} ${W},${H / 2}`;
  return (
    <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className={className} role="img"
      aria-label={`${field} difference over time, latest ${vals[vals.length - 1]}`}>
      <defs>
        <clipPath id={`${uid}-up`}><rect x="0" y="0" width={W} height={H / 2} /></clipPath>
        <clipPath id={`${uid}-down`}><rect x="0" y={H / 2} width={W} height={H / 2} /></clipPath>
      </defs>
      <polygon points={area} fill="#2dbf8d" opacity="0.25" clipPath={`url(#${uid}-up)`} />
      <polygon points={area} fill="#e5202e" opacity="0.3" clipPath={`url(#${uid}-down)`} />
      <line x1="0" x2={W} y1={H / 2} y2={H / 2} stroke="#6f6466" strokeWidth="0.5" strokeDasharray="3 3" />
      <polyline points={line} fill="none" stroke="#faf5f5" strokeWidth="1.2" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

function Metric({ label, value, sub, diff, diffText }: { label: string; value: string; sub: string; diff?: number | null; diffText?: string }) {
  return (
    <div className="rounded-md border border-ember bg-ink-900 px-3 py-2">
      <div className="text-[11px] uppercase tracking-[0.12em] text-ash-600">{label}</div>
      <div className="flex items-baseline gap-2">
        <span className="text-xl font-extrabold tabular-nums text-ash-50">{value}</span>
        {diff != null && <span className={`text-sm font-bold tabular-nums ${diffTone(diff)}`}>{diffText}</span>}
      </div>
      <div className="text-[11px] text-ash-400">{sub}</div>
    </div>
  );
}

export function goldLabel(p: LivePerformance) {
  if (!p.diff) return null;
  return { text: goldK(p.diff.gold), approx: p.gold.confidence !== 'high', title:
    `Net worth ${p.me.netWorth}g vs ~${p.opponent!.netWorth}g (±${p.gold.uncertainty}g, ${p.gold.confidence} confidence).\n` +
    `Visible items: ${signed(p.diff.itemGold)}g. Opponent's unspent gold is estimated (~${p.opponent!.unspentGold}g).` };
}

/** CS / vision / gold vs the lane opponent, streamed over /ws once per second. */
export function LivePerformancePanel({ meta, live }: { meta: Meta; live: LiveGameState }) {
  const p = live.performance;
  if (!p) return null;
  const opp = live.laneOpponent;
  const oppChamp = opp && meta.champions.find((c) => c.id === opp.championId);
  const lane = toLane(live.players.find((x) => x.isMe)?.position);
  const gold = goldLabel(p);
  const csApprox = p.csGranularity === 10 ? '≈' : '';

  return (
    <section className="animate-rise rounded-lg border border-ember bg-ink-800 p-4" aria-label="Live performance">
      <header className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <h3 className="flex items-center gap-2 text-xs font-semibold uppercase tracking-[0.14em] text-ash-400">
          <span className="relative flex size-2"><span className="absolute inline-flex size-full animate-ping rounded-full bg-[#2dbf8d] opacity-60" /><span className="relative inline-flex size-2 rounded-full bg-[#2dbf8d]" /></span>
          Live game · {clock(p.gameTime)}{lane && <> · {LANE_LABEL[lane]} lane</>}
        </h3>
        {oppChamp ? (
          <span className="flex items-center gap-2 text-xs text-ash-400">
            vs <img src={img.champ(meta.version, oppChamp.key)} alt="" className="size-6 rounded" /> <b className="text-ash-50">{oppChamp.name}</b>
          </span>
        ) : <span className="text-xs text-ash-600">No lane opponent detected — showing your stats only</span>}
      </header>

      <div className="grid gap-2 sm:grid-cols-3">
        <Metric label="Creep score" value={`${csApprox}${p.me.cs}`} sub={`${p.me.csPerMin.toFixed(1)} / min${p.opponent ? ` · opp ${p.opponent.csPerMin.toFixed(1)}` : ''}`}
          diff={p.diff?.cs} diffText={p.diff ? `${signed(p.diff.cs)} CS` : undefined} />
        <Metric label="Vision score" value={p.me.visionScore.toFixed(0)}
          sub={`${p.me.visionPerMin.toFixed(2)} / min · ${p.me.controlWards} control ward${p.me.controlWards === 1 ? '' : 's'}`}
          diff={p.diff?.vision} diffText={p.diff ? signed(p.diff.vision) : undefined} />
        <div className="rounded-md border border-ember bg-ink-900 px-3 py-2" title={gold?.title}>
          <div className="text-[11px] uppercase tracking-[0.12em] text-ash-600">Gold vs lane</div>
          <div className="flex items-baseline gap-2">
            <span className={`text-xl font-extrabold tabular-nums ${p.diff ? diffTone(p.diff.gold) : 'text-ash-50'}`}>
              {gold ? `${gold.approx ? '~' : ''}${gold.text}` : `${p.me.netWorth}g`}
            </span>
            {p.diff && <span className="text-[11px] text-ash-600">±{p.gold.uncertainty}</span>}
          </div>
          <div className="text-[11px] text-ash-400">
            {p.diff ? <>items {signed(p.diff.itemGold)}g · {p.gold.confidence} confidence</> : `${p.me.unspentGold}g unspent`}
          </div>
        </div>
      </div>
      {p.diff && <div className="mt-3"><Sparkline points={p.history} className="h-12 w-full" /></div>}
    </section>
  );
}
