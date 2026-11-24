import { useEffect, useState, type ReactNode } from 'react';
import type { ProgressInfo } from '../../../shared/types.ts';

/**
 * The app's progress bars and meters (0.10.0): a row of small vertical segments, filled from the left. The one before
 * the work's front pulses while it runs; when the work can't be counted, a soft sweep runs along the row instead
 * (still, with reduced motion). Colour is never alone: the text beside it says how far and how long.
 */
export function Segments({ fraction, count = 32, tone = 'accent', running = false, height = 'h-2.5', label, valueText }: {
  fraction: number | null; count?: number; tone?: 'accent' | 'gold'; running?: boolean; height?: string;
  /** For a progress bar (role progressbar); without one the row is decoration and the text beside it says it all. */
  label?: string; valueText?: string;
}) {
  const filled = fraction === null ? 0 : Math.min(count, Math.floor(Math.min(1, Math.max(0, fraction)) * count + 1e-9));
  const on = tone === 'gold' ? 'bg-gold-400' : 'bg-blood-400';
  const off = tone === 'gold' ? 'bg-gold-900' : 'bg-blood-800';
  const a11y = label
    ? {
      role: 'progressbar', 'aria-label': label, 'aria-valuemin': 0, 'aria-valuemax': 100,
      ...(fraction !== null ? { 'aria-valuenow': Math.round(fraction * 100) } : {}), ...(valueText ? { 'aria-valuetext': valueText } : {}),
    }
    : { 'aria-hidden': true };
  return (
    <div className="flex gap-[2px]" {...a11y}>
      {Array.from({ length: count }, (_, i) => {
        const sweep = running && fraction === null;
        const front = running && fraction !== null && i === filled && filled < count;
        return (
          <span key={i}
            className={`${height} min-w-[3px] flex-1 rounded-[1px] ${i < filled ? on : off} ${front ? `${on} motion-safe:animate-segment` : ''} ${sweep ? 'motion-safe:animate-segment' : ''}`}
            style={sweep ? { animationDelay: `${(i % 12) * 100}ms` } : undefined} />
        );
      })}
    </div>
  );
}

/** A clock that ticks every second while `on` (to count an estimate down between the server's updates). */
export function useNow(on: boolean) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!on) return;
    setNow(Date.now());
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [on]);
  return now;
}

/** A duration in words, for an estimate: "a few seconds", "about 40 s", "about a minute", "about 14 min", "about 1 h 20 min". */
export function fmtAbout(ms: number): string {
  if (ms < 10_000) return 'a few seconds';
  if (ms < 55_000) return `about ${Math.max(10, Math.round(ms / 5000) * 5)} s`;
  if (ms < 90_000) return 'about a minute';
  const min = Math.round(ms / 60_000);
  if (min < 60) return `about ${min} min`;
  const h = Math.floor(min / 60), m = min % 60;
  return `about ${h} h${m ? ` ${m} min` : ''}`;
}

/** m:ss (h:mm:ss past an hour). */
export function clock(ms: number) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
  return h ? `${h}:${String(m).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}` : `${m}:${String(s % 60).padStart(2, '0')}`;
}

/** What is left of an estimate at `now`, counted down from when the server worked it out. */
export function leftAt(p: ProgressInfo, now: number): number | null {
  return p.etaMs === null ? null : Math.max(0, p.etaMs - Math.max(0, now - p.at));
}

/**
 * How far it is: the counted fraction, else (an estimate of the whole time) the time passed over that whole, kept
 * below the end until the action says it's done.
 */
export function fractionAt(p: ProgressInfo, now: number): number | null {
  if (p.fraction !== null) return p.fraction;
  const left = leftAt(p, now);
  if (left === null) return null;
  const passed = now - p.startedAt;
  return Math.min(0.97, passed / Math.max(1, passed + left));
}

/** "about 4 min left", "taking longer than estimated", or "estimating the time left…" before there is anything to go on. */
export function leftText(p: ProgressInfo, now: number, uncounted?: string): string {
  const left = leftAt(p, now);
  if (left === null) return uncounted && p.fraction === null ? uncounted : 'estimating the time left…';
  if (left <= 0) return 'taking longer than estimated';
  return `${fmtAbout(left)} left`;
}

/**
 * A running action: what it is doing (`label`), the segmented bar, the time it has run and its time left, marked as
 * an estimate. `aside` sits on the label's line (a step count, bytes copied). `uncounted`: what to say instead of
 * "estimating the time left…" for a part that can't be counted and has nothing to estimate from.
 */
export function ProgressBar({ progress, label, aside, title, count, uncounted }: {
  progress: ProgressInfo; label: ReactNode; aside?: ReactNode; title: string; count?: number; uncounted?: string;
}) {
  const now = useNow(true);
  const fraction = fractionAt(progress, now);
  const left = leftText(progress, now, uncounted);
  const estimated = leftAt(progress, now) !== null;
  return (
    <div className="space-y-1" aria-live="polite">
      <div className="flex items-baseline justify-between gap-3 text-xs text-ash-200">
        <span className="min-w-0">{label}</span>
        {aside && <span className="shrink-0 tabular-nums text-ash-400">{aside}</span>}
      </div>
      <Segments fraction={fraction} running count={count} label={title}
        valueText={`${fraction !== null ? `${Math.round(fraction * 100)}%, ` : ''}${left}${estimated ? ' (estimate)' : ''}`} />
      <div className="flex items-baseline justify-between gap-3 text-[11px] text-ash-400">
        <span className="tabular-nums">{clock(now - progress.startedAt)}{fraction !== null && ` · ${Math.round(fraction * 100)}%`}</span>
        <span>{left}{estimated && <span className="text-ash-600"> · estimate</span>}</span>
      </div>
    </div>
  );
}

/** Before an action starts: how long it should take here, "takes about 14 min (estimate)"; nothing without an estimate. */
export function Takes({ ms, className = '' }: { ms: number | null | undefined; className?: string }) {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return null;
  return <span className={`text-[11px] text-ash-600 ${className}`}>Takes {fmtAbout(ms)} (estimate)</span>;
}
