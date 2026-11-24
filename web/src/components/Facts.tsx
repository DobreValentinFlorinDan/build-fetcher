import type { GoldAt, MatchupFacts } from '../../../shared/types.ts';
import { pct, type Meta } from '../api.ts';
import { Card, Empty } from './common.tsx';

// Stage C2: how the matchup tends to go in the stored high-elo games. Before the game only: the dashboard swaps this
// card for the live one once the game starts, and the overlay shows it in champ select.

export const goldText = (g: number) => `${g > 0 ? '+' : g < 0 ? '−' : '±'}${Math.abs(g).toLocaleString('en-US')}g`;
/** "Long Sword + Health Potion ×2". */
const itemsText = (meta: Meta, ids: number[]) => [...new Set(ids)]
  .map((id) => { const n = ids.filter((x) => x === id).length; return `${meta.items[id]?.name ?? `#${id}`}${n > 1 ? ` ×${n}` : ''}`; }).join(' + ');
export const lengthLabel = (b: { from: number; to: number | null }) => (b.to === null ? `${b.from}+ min` : b.from === 0 ? `< ${b.to} min` : `${b.from}–${b.to} min`);

function GoldRow({ label, g }: { label: string; g: GoldAt | null }) {
  if (!g) return null;
  return (
    <div className="flex items-baseline justify-between gap-2" title={`Median of ${g.games} games: Irelia's total gold minus her lane opponent's`}>
      <span className="text-ash-400">{label}</span>
      <span className="tabular-nums">
        <b className={g.median > 0 ? 'text-[#2dbf8d]' : g.median < 0 ? 'text-blood-300' : 'text-ash-50'}>{goldText(g.median)}</b>
        <span className="ml-2 text-[11px] text-ash-600">ahead in {Math.round(g.ahead * 100)}%</span>
      </span>
    </div>
  );
}

export function FactsCard({ meta, facts, opponentId }: { meta: Meta; facts: MatchupFacts | null | undefined; opponentId: number | null }) {
  const opp = opponentId !== null ? meta.champions.find((c) => c.id === opponentId) : undefined;
  if (!facts) {
    return (
      <Card title="Matchup">
        <Empty>{opp ? `Too few stored games against ${opp.name} in this lane yet.` : 'Pick a lane opponent to see how the matchup tends to go.'}</Empty>
      </Card>
    );
  }
  const most = Math.max(...facts.byLength.map((b) => b.games));
  return (
    <Card title={`Matchup${opp ? ` vs ${opp.name}` : ''}`} aside={<span className="text-[11px] text-ash-600">{facts.games} games · {facts.pilots} pilots</span>}>
      <div className="space-y-1 text-[13px]">
        <GoldRow label="Lane gold at 10:00" g={facts.gold10} />
        <GoldRow label="Lane gold at 15:00" g={facts.gold15} />
        <div className="flex items-baseline justify-between gap-2"><span className="text-ash-400">Win rate</span><b className="tabular-nums text-ash-50">{pct(facts.winRate, 0)}</b></div>
      </div>
      <h3 className="mt-3 text-[11px] uppercase tracking-wider text-ash-600">Win rate by game length</h3>
      <ul className="mt-1 space-y-1">
        {facts.byLength.map((b) => (
          <li key={b.from} className="grid grid-cols-[64px_1fr_auto] items-center gap-2 text-[11px]" title={`${b.games} games`}>
            <span className="text-ash-400">{lengthLabel(b)}</span>
            <div className="h-1.5 overflow-hidden rounded-full bg-ink-900">
              <div className={`h-full ${b.winRate >= 0.5 ? 'bg-[#2dbf8d]' : 'bg-blood-500'}`} style={{ width: `${Math.max(4, b.winRate * 100)}%`, opacity: 0.4 + 0.6 * (b.games / most) }} />
            </div>
            <span className="w-16 text-right tabular-nums text-ash-200">{pct(b.winRate, 0)} <span className="text-ash-600">({b.games})</span></span>
          </li>
        ))}
      </ul>
      {facts.best && (facts.best.start || facts.best.first.length > 0) && (
        <>
          <h3 className="mt-3 text-[11px] uppercase tracking-wider text-ash-600">Did best in this matchup</h3>
          <div className="mt-1 space-y-1 text-[12px]"
            title="Win-probability-added in these games, shrunk towards zero so a few lucky games can't top the list (estimates). Saved to the League client as the item set's vs blocks when you import the build.">
            {facts.best.start && (
              <div className="flex items-baseline justify-between gap-2">
                <span className="text-ash-400">Start</span>
                <span className="text-right text-ash-50">{itemsText(meta, facts.best.start.value)} <span className="text-ash-600">({facts.best.start.games})</span></span>
              </div>
            )}
            {facts.best.first.length > 0 && (
              <div className="flex items-baseline justify-between gap-2">
                <span className="text-ash-400">First item</span>
                <span className="text-right text-ash-50">{facts.best.first.map((o) => `${meta.items[o.value]?.name ?? `#${o.value}`} (${o.games})`).join(' · ')}</span>
              </div>
            )}
          </div>
        </>
      )}
      <p className="mt-2 text-[11px] text-ash-600">High-elo Irelia games in this lane against this opponent. Before the game only.</p>
    </Card>
  );
}

/** One line for the overlay in champ select: "−320g at 10 · −540g at 15 · 46 games". */
export function factsLine(f: MatchupFacts): string {
  return [f.gold10 && `${goldText(f.gold10.median)} at 10`, f.gold15 && `${goldText(f.gold15.median)} at 15`, `${pct(f.winRate, 0)} WR`, `${f.games} games`]
    .filter(Boolean).join(' · ');
}
