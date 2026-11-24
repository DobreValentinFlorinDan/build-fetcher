// /overlay: the in-game HUD (a transparent, always-on-top, click-through window in the desktop app). It highlights
// choices and leaves them to the player: next buys and counter items are always several ranked options with the games
// behind them. Nothing pops up, flashes or alerts, and no timer is tracked.
import { useEffect, useLayoutEffect, useRef } from 'react';
import { useServer } from './api.ts';
import { Evidence, Img } from './components.tsx';
import { useDd } from './ddragon.ts';
import { desktop } from './desktop.ts';
import { pct, ROLE_NAMES } from './format.ts';
import type { RankedOption } from './types.ts';

function Row({ o, reason }: { o: RankedOption; reason?: string }) {
  const dd = useDd();
  const id = Number(o.option.key.split(/[,>]/)[0]);
  return (
    <li className="flex items-center gap-2" title={`${o.label}${reason ? ` — ${reason}` : ''}`}>
      <Img src={dd.itemIcon(id)} title={dd.itemName(id)} size={26} />
      <div className="min-w-0">
        <div className="truncate text-[12px] text-fg">{o.label}</div>
        <Evidence o={o} compact />
        {reason && <div className="truncate text-[11px] text-muted">{reason}</div>}
      </div>
    </li>
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
          <section>
            <h3 className="mb-1 text-[10px] uppercase tracking-wider text-muted">Next buys to consider</h3>
            <ol className="space-y-1.5">{advice.nextBuys.slice(0, 4).map((o) => <Row key={o.option.key} o={o} />)}</ol>
          </section>
          {advice.counters.length > 0 && (
            <section>
              <h3 className="mb-1 text-[10px] uppercase tracking-wider text-muted">Answers to the enemy build</h3>
              <ol className="space-y-1.5">{advice.counters.slice(0, 3).map(([o, why]) => <Row key={o.option.key} o={o} reason={why} />)}</ol>
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
