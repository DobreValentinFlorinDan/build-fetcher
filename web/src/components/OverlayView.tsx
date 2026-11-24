import { useEffect, useRef, useState } from 'react';
import type { DesktopSettings, OverlayState } from '../../../shared/desktop.ts';
import { buildPlan, planStatus, type PlanStatus } from '../../../shared/buildPlan.ts';
import { api, img, pct, useRealtime, type Meta } from '../api.ts';
import { desktop } from '../desktop.ts';
import { LANE_LABEL, laneNote } from '../lane.ts';
import { factsLine } from './Facts.tsx';
import { clock, diffTone, goldLabel, signed, Sparkline } from './LivePerformance.tsx';
import { Segments } from './Progress.tsx';

// In-game HUD served at /overlay. The desktop shell shows it in a transparent, frameless, always-on-top
// window above League (Borderless mode); it is a plain web page, nothing is injected into the game.
// Opened in a normal browser it renders the same HUD for previewing. It shows the matchup and build the server pushes
// (server/matchup.ts), the same the dashboard shows: an opponent or lane picked there, or the Draft-adjusted / Lane
// baseline switch, reaches the overlay at once.

function Row({ label, value, diff, diffText }: { label: string; value: string; diff?: number; diffText?: string }) {
  return (
    <div className="flex items-baseline justify-between gap-2">
      <span className="text-ash-400">{label}</span>
      <span className="tabular-nums">
        <b className="text-ash-50">{value}</b>
        {diff !== undefined && <b className={`ml-1.5 ${diffTone(diff)}`}>{diffText}</b>}
      </span>
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="border-t border-ember px-3 py-2">
      <div className="mb-1 text-[10px] font-semibold uppercase tracking-[0.14em] text-ash-600">{title}</div>
      {children}
    </div>
  );
}

/** Hover text for one plan entry: what fills it, or what is still missing. */
function entryTitle(x: PlanStatus, name: (id: number) => string) {
  const planned = `${x.slot}: ${name(x.id)}`;
  switch (x.state) {
    case 'done': return `${planned} ✓`;
    case 'upgraded': return `${x.slot}: ${name(x.owned!)} (upgraded ${name(x.id)}) ✓`;
    case 'swap': return `${x.slot}: ${name(x.owned!)} instead of ${name(x.id)}`;
    case 'building': return `${planned}: building (${x.components.map(name).join(', ')}), ${x.cost}g to go`;
    case 'todo': return planned;
  }
}

/** Gold still to pay for `id`: its price minus the components of its recipe you own (each owned item counted once). */
function costLeft(id: number, owned: number[], items: Meta['items']): number {
  const pool = [...owned];
  const covered = (x: number): number => {
    const at = pool.indexOf(x);
    if (at >= 0) { pool.splice(at, 1); return items[x]?.gold ?? 0; }
    return (items[x]?.from ?? []).reduce((sum, c) => sum + covered(c), 0);
  };
  const price = items[id]?.gold ?? 0;
  return Math.max(0, price - (items[id]?.from ?? []).reduce((sum, c) => sum + covered(c), 0));
}

/** The start's items in order, each with how many: Long Sword, Health Potion ×3. */
function startItems(ids: number[]): { id: number; n: number }[] {
  const out: { id: number; n: number }[] = [];
  for (const id of ids) {
    const e = out.find((x) => x.id === id);
    if (e) e.n++; else out.push({ id, n: 1 });
  }
  return out;
}

/** The start stays on the overlay this long into a game (the buy itself happens at 0:00); bought, it shows ✓. */
const START_SHOWN_MS = 3 * 60_000;
/** Past this, the start counts as bought even when only consumables were in it (they may be used up already). */
const START_SHOP_MS = 90_000;

export function OverlayView() {
  const rt = useRealtime();
  const [meta, setMeta] = useState<Meta | null>(null);
  const [ov, setOv] = useState<OverlayState | null>(null);
  const [hotkeys, setHotkeys] = useState<DesktopSettings['hotkeys'] | null>(null);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    document.documentElement.classList.add('overlay-mode');
    document.title = 'Irelia Companion Overlay';
  }, []);
  useEffect(() => { api.meta().then(setMeta).catch(() => {}); }, [rt.patchVersion]);

  const live = rt.live;
  const m = rt.matchup;
  const res = rt.build?.response ?? null;
  const draftMode = res?.mode === 'draft' && !!res.draft;
  const rec = res ? (draftMode ? res.draft!.build : res.baseline) : null;
  const lane = m?.lane ?? 'TOP';
  const oppId = m?.opponentId ?? null;

  useEffect(() => {
    if (!desktop) return;
    desktop.overlay.state().then(setOv).catch(() => {});
    desktop.settings.get().then((s) => setHotkeys(s.hotkeys)).catch(() => {});
    return desktop.overlay.onState(setOv);
  }, []);

  // The transparent window hugs the HUD: less screen covered, less to composite.
  useEffect(() => {
    const el = root.current;
    if (!desktop || !el) return;
    const ro = new ResizeObserver(() => desktop!.overlay.fitHeight(Math.ceil(el.getBoundingClientRect().height)));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const interactive = ov ? !ov.clickThrough : !desktop;
  const p = live?.performance ?? null;
  const me = live?.players.find((x) => x.isMe);
  // Plan and ownership rules: shared/buildPlan.ts (upgrades, other boots and same-group items fill a slot).
  const { entries: plan, next } = rec && meta ? planStatus(buildPlan(rec, meta.items), me?.items ?? [], meta.items) : { entries: [], next: null };
  const nextCost = next?.cost ?? 0;
  // The start to buy (shop advice, 0.11.0): before the game and its first minutes; bought once you own one of its items.
  const start = rec ? startItems(rec.startingItems[0]?.value ?? []) : [];
  const startBought = !!live && (start.some((x) => me?.items.includes(x.id)) || live.gameTime * 1000 > START_SHOP_MS);
  const startShown = start.length > 0 && (!live || live.gameTime * 1000 < START_SHOWN_MS);
  // Until it's bought, the start is the buy: "With 500g: …" (the best buy for your gold) waits.
  const buyingStart = !!live && startShown && !startBought;
  const itemName = (id: number) => meta?.items[id]?.name ?? `#${id}`;
  const opp = oppId && meta ? meta.champions.find((c) => c.id === oppId) : undefined;
  const runes = rec?.runes?.value;
  const keystone = runes && meta?.runes.flatMap((t) => t.slots[0]?.runes ?? []).find((r) => r.id === runes.perks[0]);
  const gold = p ? goldLabel(p) : null;

  return (
    <div ref={root} className={`w-[300px] select-none overflow-hidden rounded-lg border bg-ink-950/85 pb-1 text-[12px] text-ash-200 shadow-2xl ${interactive ? 'border-blood-500' : 'border-blood-900/70'}`}>
      <header className={`flex items-center gap-2 px-3 py-1.5 ${interactive ? 'cursor-move bg-blood-900/50 [-webkit-app-region:drag]' : ''}`}>
        <span className={`size-1.5 rounded-full ${rt.connected ? 'bg-[#2dbf8d]' : 'bg-ash-600'}`} />
        <b className="text-[11px] tracking-[0.2em] text-blood-300">IRELIA</b>
        <span className="tabular-nums text-ash-400">{live ? clock(live.gameTime) : m?.phase === 'draft' ? 'Champ select' : 'Waiting for a game'}</span>
        {interactive && desktop && (
          <span className="ml-auto flex gap-1 [-webkit-app-region:no-drag]">
            <button type="button" onClick={() => void desktop!.overlay.setClickThrough(true)}
              className="rounded border border-ink-600 px-1.5 text-[10px] text-ash-200 hover:border-blood-500">Lock</button>
            <button type="button" onClick={() => void desktop!.overlay.setVisible(false)} aria-label="Hide overlay"
              className="rounded border border-ink-600 px-1.5 text-[10px] text-ash-200 hover:border-blood-500">✕</button>
          </span>
        )}
      </header>

      {meta && rec && (
        <Section title={`${LANE_LABEL[lane]} · ${opp ? `vs ${opp.name}` : 'all matchups'} · ${draftMode ? 'draft-adjusted' : 'lane build'}`}>
          <div className="flex items-center gap-2">
            {opp && <img src={img.champ(meta.version, opp.key)} alt="" className="size-8 rounded" />}
            {keystone && <img src={img.rune(keystone.icon)} alt={keystone.name} title={keystone.name} className="size-8 rounded-full bg-black" />}
            <div className="min-w-0 leading-tight">
              <div>
                {rec.winRate != null && <><b className="text-ash-50">{pct(rec.winRate)}</b> <span className="text-ash-400">WR · </span></>}
                <span className="text-ash-400">{opp ? `${rec.sample.matchupGames} ${opp.name} games` : `${rec.sample.laneGames} ${LANE_LABEL[lane]} games`}</span>
              </div>
              <div className="truncate text-ash-400">
                {keystone?.name}{rec.skills && <> · max {rec.skills.value.priority.join(' › ')}</>}
              </div>
            </div>
          </div>
          {rec.sample.totalGames === 0
            ? <div className="mt-1 text-[10px] text-gold-400">No Irelia games stored yet: start fetching in the app</div>
            : <>
              {rec.sample.usedFallback && opp && <div className="mt-1 text-[10px] text-gold-400">Few {opp.name} games: blended with all {LANE_LABEL[lane]} matchups</div>}
              {laneNote(rec) && <div className="mt-1 text-[10px] text-gold-400">{laneNote(rec)}</div>}
              {/* Matchup facts (Stage C2) before the game only. */}
              {!live && m?.phase !== 'game' && res?.facts && (
                <div className="mt-1 text-[10px] text-ash-400" title="Lane gold difference (median) and win rate in stored high-elo games of this matchup">
                  {opp ? `vs ${opp.name}: ` : ''}{factsLine(res.facts)}
                </div>
              )}
            </>}
        </Section>
      )}

      {p && (
        <Section title="Lane performance">
          <div className="space-y-0.5">
            <Row label="CS" value={`${p.csGranularity === 10 ? '≈' : ''}${p.me.cs} · ${p.me.csPerMin.toFixed(1)}/m`}
              diff={p.diff?.cs} diffText={p.diff ? signed(p.diff.cs) : undefined} />
            <Row label="Vision" value={`${p.me.visionScore.toFixed(0)} · ${p.me.controlWards} CW`}
              diff={p.diff?.vision} diffText={p.diff ? signed(p.diff.vision) : undefined} />
            {gold ? (
              <div className="flex items-baseline justify-between gap-2" title={gold.title}>
                <span className="text-ash-400">Gold</span>
                <span className="tabular-nums">
                  <b className={diffTone(p.diff!.gold)}>{gold.approx ? '~' : ''}{gold.text}</b>
                  <span className="ml-1.5 text-[10px] text-ash-600">±{p.gold.uncertainty}</span>
                </span>
              </div>
            ) : <Row label="Net worth" value={`${p.me.netWorth}g`} />}
          </div>
          {p.diff && <Sparkline points={p.history} className="mt-1 h-7 w-full" />}
        </Section>
      )}

      {meta && live?.summary && (live.summary.fedEnemy || live.summary.fedAlly || live.summary.teamItems !== 0) && (
        <Section title="Game">
          <div className="space-y-0.5" title="From the scoreboard: the item value everyone can see on Tab, and kills. Estimates.">
            <Row label="Team items" value={`${live.summary.teamItems >= 0 ? '+' : '−'}${(Math.abs(live.summary.teamItems) / 1000).toFixed(1)}k`} />
            {live.summary.fedEnemy && <Row label="Most fed enemy" value={`${meta.champions.find((c) => c.id === live.summary!.fedEnemy!.championId)?.name ?? '?'} · ${live.summary.fedEnemy.ap ? 'AP' : 'AD'} ${(live.summary.fedEnemy.items / 1000).toFixed(1)}k`} />}
            {live.summary.fedAlly && <Row label="Most fed ally" value={`${live.summary.fedAlly.isMe ? 'You' : meta.champions.find((c) => c.id === live.summary!.fedAlly!.championId)?.name ?? '?'} ${(live.summary.fedAlly.items / 1000).toFixed(1)}k`} />}
          </div>
        </Section>
      )}

      {meta && rec && startShown && (
        <Section title={buyingStart ? 'Buy first' : 'Start'}>
          <div className="flex items-center justify-between gap-2" title={`The start this build recommends: ${start.map((x) => `${itemName(x.id)}${x.n > 1 ? ` ×${x.n}` : ''}`).join(', ')}`}>
            <span className="flex min-w-0 flex-wrap items-center gap-1">
              {start.map((x) => (
                <span key={x.id} className="relative">
                  <img src={img.item(meta.version, x.id)} alt={itemName(x.id)}
                    className={`size-7 rounded border ${buyingStart ? 'border-gold-400' : 'border-ink-600'} ${startBought ? 'opacity-35 grayscale' : ''}`} />
                  {x.n > 1 && <span className="absolute -right-1 -bottom-1 rounded bg-ink-950 px-0.5 text-[10px] font-bold tabular-nums text-ash-50">×{x.n}</span>}
                  {startBought && <span className="absolute -left-0.5 -bottom-0.5 text-[10px] text-[#2dbf8d]">✓</span>}
                </span>
              ))}
            </span>
            <span className="shrink-0 truncate text-[11px] text-ash-400">
              {start.map((x) => `${itemName(x.id)}${x.n > 1 ? ` ×${x.n}` : ''}`).join(' + ')}
            </span>
          </div>
        </Section>
      )}

      {meta && rec && plan.length > 0 && (
        <Section title="Build order">
          <div className="flex flex-wrap items-center gap-1">
            {plan.map((x) => {
              const filled = x.state === 'done' || x.state === 'upgraded' || x.state === 'swap';
              const shown = x.owned ?? x.id;   // a slot filled by another item shows that item
              return (
                <span key={`${x.slot}-${x.id}`} className="relative" title={entryTitle(x, itemName)}>
                  <img src={img.item(meta.version, shown)} alt={itemName(shown)}
                    className={`size-7 rounded border ${x === next ? 'border-gold-400' : x.state === 'building' ? 'border-dashed border-gold-400/60' : 'border-ink-600'} ${filled ? 'opacity-35 grayscale' : ''}`} />
                  {filled && <span className={`absolute -right-0.5 -bottom-0.5 text-[10px] ${x.state === 'swap' ? 'text-gold-400' : 'text-[#2dbf8d]'}`}>{x.state === 'swap' ? '⇄' : '✓'}</span>}
                </span>
              );
            })}
          </div>
          {draftMode && live?.next && (() => {
            const cost = costLeft(live.next.itemId, me?.items ?? [], meta.items);
            return (
              <div className="mt-1.5 leading-tight" title="Re-ranked from the live game (estimate). Build advice only.">
                <div className="flex items-center justify-between gap-1.5 text-[11px]">
                  <span className="flex min-w-0 items-center gap-1.5">
                    <img src={img.item(meta.version, live.next.itemId)} alt="" className="size-5 rounded" />
                    <span className="truncate text-ash-50">Next: <b>{itemName(live.next.itemId)}</b></span>
                  </span>
                  <span className="shrink-0 tabular-nums text-ash-400">{Math.floor(live.myGold)} / {cost}g</span>
                </div>
                {live.next.reason && <div className="truncate text-[10px] text-gold-400">adjusted for: {live.next.reason}</div>}
                {cost > 0 && (
                  <div className="mt-0.5"><Segments fraction={Math.min(1, live.myGold / cost)} count={20} height="h-1.5" tone={live.myGold >= cost ? 'gold' : 'accent'} /></div>
                )}
              </div>
            );
          })()}
          {live?.buy && !buyingStart && (
            <div className="mt-1.5 flex items-center gap-1.5 leading-tight"
              title={`${live.buy.reason}${live.buy.lift !== null && live.buy.games ? ` · those games did ${(live.buy.lift * 100).toFixed(1)} pts against the game state (estimate)` : ''}`}>
              <span className="flex shrink-0 gap-0.5">{live.buy.items.map((id, i) => <img key={`${id}-${i}`} src={img.item(meta.version, id)} alt="" className="size-5 rounded" />)}</span>
              <span className="min-w-0 truncate text-[11px]">
                <span className="text-gold-400">With {Math.floor(live.buy.gold)}g:</span>{' '}
                <span className="text-ash-50">{live.buy.items.map(itemName).join(' + ')}</span>
              </span>
            </div>
          )}
          {next && !(draftMode && live?.next) && (
            <div className="mt-1.5">
              <div className="flex justify-between text-[11px]">
                <span className="truncate text-ash-50">Next: <b>{itemName(next.id)}</b></span>
                {live && <span className="tabular-nums text-ash-400"
                  title={next.components.length ? `${meta.items[next.id]?.gold}g minus the ${next.components.map(itemName).join(', ')} you own` : undefined}>
                  {Math.floor(live.myGold)} / {nextCost}g
                </span>}
              </div>
              {live && nextCost > 0 && (
                <div className="mt-0.5"><Segments fraction={Math.min(1, live.myGold / nextCost)} count={20} height="h-1.5" tone={live.myGold >= nextCost ? 'gold' : 'accent'} /></div>
              )}
            </div>
          )}
        </Section>
      )}

      {/* Counter items for the enemy team go with the lane build; the draft-adjusted "Next" already weighs them. */}
      {meta && live && live.suggestions.length > 0 && !(draftMode && live.next) && (
        <Section title="Adapt">
          {live.suggestions.slice(0, 2).map((s) => {
            const id = s.replacement ?? s.itemId;
            return (
              <div key={s.itemId} className="flex items-center gap-2 py-0.5">
                <img src={img.item(meta.version, id)} alt="" className="size-6 rounded" />
                <div className="min-w-0 leading-tight">
                  <div className="truncate font-semibold text-ash-50">{meta.items[id]?.name}</div>
                  <div className="truncate text-[10px] text-ash-400">{s.reason}</div>
                </div>
              </div>
            );
          })}
        </Section>
      )}

      {desktop && hotkeys && (
        <div className="border-t border-ember px-3 py-1 text-[10px] text-ash-600">
          {hotkeys.toggleOverlay} hide · {hotkeys.toggleClickThrough} {interactive ? 'lock' : 'unlock / move'}
        </div>
      )}
    </div>
  );
}
