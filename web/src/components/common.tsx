import { useEffect, useId, useState, type ReactNode } from 'react';
import { img, pct, wrClass, type Meta } from '../api.ts';

/**
 * A status line that never changes the layout (0.18.0): always one line high, present even when empty, and cut short
 * with "…" instead of wrapping (the whole text is in the tooltip). Lines that came and went, or grew to two lines,
 * moved everything below them several times a second while fetching.
 */
export function StatusSlot({ children, title, className = '' }: { children?: ReactNode; title?: string; className?: string }) {
  const empty = children === null || children === undefined || children === false || children === '';
  return (
    <div className={`status-slot ${className}`} title={empty ? undefined : title ?? (typeof children === 'string' ? children : undefined)}
      aria-hidden={empty || undefined}>
      {empty ? ' ' : children}
    </div>
  );
}

// ------------------------------------------------------------------ Show details (0.18.0)

const DETAILS_KEY = 'showDetails';
const DETAILS_EVENT = 'irelia:show-details';
const readDetails = () => { try { return localStorage.getItem(DETAILS_KEY) !== '0'; } catch { return true; } };

/**
 * Whether the log-like lines are shown: the last event, your account, the per-server lines and the GPU log. Hiding them
 * only hides them: everything behind them keeps running. Remembered, and the same everywhere in the window.
 */
export function useShowDetails(): [boolean, (on: boolean) => void] {
  const [on, setOn] = useState(readDetails);
  useEffect(() => {
    const sync = () => setOn(readDetails());
    window.addEventListener(DETAILS_EVENT, sync);
    window.addEventListener('storage', sync);
    return () => { window.removeEventListener(DETAILS_EVENT, sync); window.removeEventListener('storage', sync); };
  }, []);
  const set = (v: boolean) => {
    try { localStorage.setItem(DETAILS_KEY, v ? '1' : '0'); } catch { /* private mode: this window only */ }
    setOn(v);
    window.dispatchEvent(new Event(DETAILS_EVENT));
  };
  return [on, set];
}

/** The small "Show details / Hide details" button. */
export function DetailsToggle({ className = '' }: { className?: string }) {
  const [on, set] = useShowDetails();
  return (
    <button type="button" className={`ghost !px-2 !py-0.5 text-[11px] ${className}`} aria-pressed={on} onClick={() => set(!on)}
      title={on ? 'Hide the log lines (fetching and logging go on)' : 'Show the log lines'}>
      {on ? 'Hide details' : 'Show details'}
    </button>
  );
}

export function ItemIcon({ meta, id, size = 40 }: { meta: Meta; id: number; size?: number }) {
  const item = meta.items[id];
  const title = item ? `${item.name} — ${item.gold}g${item.groups.length ? `\nUnique: ${item.groups.join(', ')}` : ''}` : `Item #${id}`;
  return <img className="icon item" src={img.item(meta.version, id)} width={size} height={size} alt={item?.name ?? String(id)} title={title} />;
}

export function Card({ title, aside, children, className = '' }: { title: string; aside?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={`card ${className}`}>
      <header className="card-head">
        <h2>{title}</h2>
        {aside && <div className="card-aside">{aside}</div>}
      </header>
      {children}
    </section>
  );
}

export function WinStat({ winRate, games }: { winRate: number; games: number }) {
  const weighted = !Number.isInteger(games);
  return (
    <div className="winstat" title={weighted ? `${games.toFixed(1)} games after weighting (previous patch, other lane or matchups, pilots)` : undefined}>
      <span className={`wr ${wrClass(winRate)}`}>{pct(winRate)} WR</span>
      <span className="games">{fmtGames(games)} games</span>
    </div>
  );
}

/** Game counts are often weighted (fractional): show them whole, "≈" when rounded. */
export const fmtGames = (g: number) => `${Number.isInteger(g) ? '' : '≈'}${Math.round(g).toLocaleString('en-US')}`;

/** An on / off switch: the app's one control for settings that are either on or off. */
export function Switch({ checked, onChange, disabled, labelledBy, label }: {
  checked: boolean; onChange: (on: boolean) => void; disabled?: boolean; labelledBy?: string; label?: string;
}) {
  return (
    <button type="button" role="switch" aria-checked={checked} aria-labelledby={labelledBy} aria-label={label} disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full border transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-blood-500 disabled:cursor-not-allowed disabled:opacity-40 ${
        checked ? 'border-blood-500 bg-blood-600' : 'border-ink-600 bg-ink-950 hover:border-ash-600'}`}>
      <span className={`absolute left-0.5 size-3.5 rounded-full transition-transform ${checked ? 'translate-x-4 bg-ash-50' : 'bg-ash-400'}`} />
    </button>
  );
}

/** A setting on one line: what it is (and, below, what it does or where it stands) on the left, its switch on the right. */
export function SwitchRow({ label, hint, checked, onChange, disabled }: {
  label: ReactNode; hint?: ReactNode; checked: boolean; onChange: (on: boolean) => void; disabled?: boolean;
}) {
  const id = useId();
  return (
    <div className="flex items-start justify-between gap-4 py-1.5">
      <div className="min-w-0">
        <div id={id} className="text-sm text-ash-200">{label}</div>
        {hint && <div className="mt-0.5 text-[11px] leading-snug text-ash-600">{hint}</div>}
      </div>
      <Switch checked={checked} onChange={onChange} disabled={disabled} labelledBy={id} />
    </div>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="empty">{children}</div>;
}
