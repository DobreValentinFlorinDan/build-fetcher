// /overlay: the in-game HUD (a transparent, always-on-top, click-through window in the desktop app). It highlights
// choices and leaves them to the player: next buys and counter items are always several ranked options with the games
// behind them. Nothing pops up, flashes or alerts, and no timer is tracked. Since 1.2.0 each option also shows its
// price against the user's own gold, the part to buy now, and what in the enemy's items (as the scoreboard shows
// them) makes it fit. The enemy's gold is never shown or estimated: the game doesn't show it. Since 1.5.0 the items read
// as a build order: the starting items at the start, the items built, then each next buy until the build is full,
// following the user's gold, the components already bought and the enemy's and lane opponent's items. Each step keeps
// other items for its slot beside it, so the choice stays the player's.
import { useEffect, useLayoutEffect, useRef } from 'react';
import { useServer } from './api.ts';
import { Evidence, Img } from './components.tsx';
import { useDd } from './ddragon.ts';
import { desktop } from './desktop.ts';
import { goldLine, pct, ROLE_NAMES } from './format.ts';
import type { BuildPlan, BuyNote, RankedOption } from './types.ts';

function Row({ o, reason, buy, first }: { o: RankedOption; reason?: string; buy?: BuyNote; first?: boolean }) {
  const dd = useDd();
  const id = Number(o.option.key.split(/[,>]/)[0]);
  const why = reason ?? buy?.reason ?? undefined;
  return (
    <li className={`flex items-center gap-2 ${first ? 'rounded border-l-2 border-accent pl-1.5' : ''}`} title={`${o.label}${why ? ` — ${why}` : ''}`}>
      <Img src={dd.itemIcon(id)} title={dd.itemName(id)} size={26} />
      <div className="min-w-0">
        <div className="truncate text-[12px] text-fg">{o.label}</div>
        <Evidence o={o} compact />
        {buy && <div className={`truncate text-[11px] ${buy.short === 0 ? 'text-good' : 'text-muted'}`}>{goldLine(buy, (p) => dd.itemName(p))}</div>}
        {why && <div className="truncate text-[11px] text-muted">{why}</div>}
      </div>
    </li>
  );
}

/** "1055,2003,2003" → [[1055, 1], [2003, 2]]. */
function counted(key: string): [number, number][] {
  const out: [number, number][] = [];
  for (const id of key.split(',').map(Number).filter((x) => x > 0)) {
    const hit = out.find((x) => x[0] === id);
    if (hit) hit[1] += 1;
    else out.push([id, 1]);
  }
  return out;
}

function Plan({ plan }: { plan: BuildPlan }) {
  const dd = useDd();
  const total = plan.done.length + plan.steps.length;
  return (
    <div className="space-y-2">
      {plan.start && (
        <section>
          <h3 className="mb-1 text-[10px] uppercase tracking-wider text-muted">Start with</h3>
          <div className="flex items-center gap-1.5" title={plan.start.label}>
            {counted(plan.start.option.key).map(([id, n]) => (
              <span key={id} className="flex items-center gap-0.5">
                <Img src={dd.itemIcon(id)} title={dd.itemName(id)} size={22} />
                {n > 1 && <span className="text-[11px] text-muted">×{n}</span>}
              </span>
            ))}
            <span className="truncate text-[11px] text-muted">{plan.start.label}</span>
          </div>
        </section>
      )}
      {plan.done.length > 0 && (
        <section className="flex items-center gap-1">
          <span className="mr-1 text-[10px] uppercase tracking-wider text-muted">Built {plan.done.length}/{Math.max(total, plan.done.length)}</span>
          {plan.done.map((id) => <Img key={id} src={dd.itemIcon(id)} title={dd.itemName(id)} size={18} />)}
        </section>
      )}
      {plan.steps.length > 0 && (
        <section>
          <h3 className="mb-1 text-[10px] uppercase tracking-wider text-muted">Build order from here</h3>
          <ol className="space-y-1.5">
            {plan.steps.map((s, i) => (
              <li key={s.item} className={`flex items-start gap-2 ${i === 0 ? 'rounded border-l-2 border-accent pl-1.5' : ''}`} title={`${s.label} — ${s.why}`}>
                <span className="mt-1 w-3 text-right text-[10px] text-muted">{plan.done.length + i + 1}</span>
                <Img src={dd.itemIcon(s.item)} title={dd.itemName(s.item)} size={i === 0 ? 26 : 22} />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-1">
                    <span className="truncate text-[12px] text-fg">{s.label}</span>
                    {s.counter && <span className="shrink-0 text-[10px] text-warn">answer</span>}
                  </div>
                  <div className={`truncate text-[11px] ${s.buy.short === 0 ? 'text-good' : 'text-muted'}`}>
                    {i === 0 ? goldLine(s.buy, (p) => dd.itemName(p)) : s.buy.short === 0 ? `Affordable after the above (${s.buy.cost})` : `${s.buy.cost} gold`}
                  </div>
                  <div className="truncate text-[11px] text-muted">{s.why}</div>
                </div>
                {i === 0 && s.alternatives.length > 0 && (
                  <div className="flex shrink-0 items-center gap-0.5 self-center opacity-80">
                    <span className="text-[10px] text-muted">or</span>
                    {s.alternatives.map((a) => <Img key={a} src={dd.itemIcon(a)} title={dd.itemName(a)} size={18} />)}
                  </div>
                )}
              </li>
            ))}
          </ol>
        </section>
      )}
      {plan.steps.length === 0 && plan.done.length > 0 && <p className="text-[12px] text-muted">Build complete.</p>}
    </div>
  );
}

export function Overlay() {
  const server = useServer();
  const dd = useDd();
  const ref = useRef<HTMLDivElement>(null);
  const live = server.live;
  // The window follows the HUD's height, so no transparent area sits over the game.
  useLayoutEffect(() => {
    if (ref.current) desktop?.overlay.fitHeight(ref.current.getBoundingClientRect().height + 2);
  });
  useEffect(() => {
    document.documentElement.style.background = 'transparent';
    document.body.style.background = 'transparent';
  }, []);
  const ctx = live?.context;
  const advice = live?.advice;
  // With a plan, only the answers it doesn't already list.
  const inPlan = new Set(advice?.plan?.steps.map((s) => s.item) ?? []);
  const counters = (advice?.counters ?? []).filter(([o]) => !inPlan.has(Number(o.option.key.split(/[,>]/)[0])));
  const buyFor = (o: RankedOption) => advice?.buys?.find((b) => b.item === Number(o.option.key.split(/[,>]/)[0]));
  return (
    <div ref={ref} className="w-[300px] select-none rounded-lg border border-line/70 bg-bg/85 p-2.5 text-fg">
      <header className="mb-2 flex items-center justify-between text-[11px] text-muted">
        <span className="font-semibold tracking-widest text-accent">BUILD FETCHER</span>
        {ctx && <span>{dd.championName(ctx.draft.champion)}{ctx.draft.role ? ` · ${ROLE_NAMES[ctx.draft.role]}` : ''}</span>}
      </header>
      {!live && <p className="text-[12px] text-muted">{server.connected ? 'Waiting for a game.' : 'Connecting…'}</p>}
      {live && !advice && <p className="text-[12px] text-muted">{live.error ?? 'Reading the game…'}</p>}
      {advice && (
        <div className="space-y-2.5">
          {advice.plan ? (
            <Plan plan={advice.plan} />
          ) : (
            <section>
              <h3 className="mb-1 text-[10px] uppercase tracking-wider text-muted">Next buys to consider</h3>
              <ol className="space-y-1.5">
                {advice.nextBuys.slice(0, 3).map((o, i) => (
                  <Row key={o.option.key} o={o} first={i === 0} buy={buyFor(o)} />
                ))}
              </ol>
            </section>
          )}
          {counters.length > 0 && (
            <section>
              <h3 className="mb-1 text-[10px] uppercase tracking-wider text-muted">{advice.plan ? 'Other answers to the enemy build' : 'Answers to the enemy build'}</h3>
              <ol className="space-y-1.5">{counters.slice(0, advice.plan ? 2 : 3).map(([o, why]) => <Row key={o.option.key} o={o} reason={why} buy={buyFor(o)} />)}</ol>
            </section>
          )}
          {advice.predictedWin != null && <p className="text-[11px] text-muted">Model's win chance from here: {pct(advice.predictedWin, 0)}</p>}
          {advice.notes.length > 0 && <p className="text-[10px] text-muted">{advice.notes[0]}</p>}
        </div>
      )}
      {ctx && live?.performance?.diff && (
        <footer className="mt-2 flex gap-3 border-t border-line/60 pt-1.5 text-[11px] text-muted">
          <span>CS {ctx.myCs}{live.performance.diff.cs !== 0 && ` (${live.performance.diff.cs > 0 ? '+' : ''}${live.performance.diff.cs})`}</span>
          <span>Kills {ctx.teamKills}–{ctx.enemyKills}</span>
          <span>Towers {ctx.towers[0]}–{ctx.towers[1]}</span>
        </footer>
      )}
      {live?.simulated && <p className="mt-1 text-[10px] text-warn">simulated game</p>}
    </div>
  );
}
