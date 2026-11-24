// How the dashboard reads a pacing snapshot (types.ts RateSnapshot). Pure functions, shared so they can be
// unit-tested next to the pacer (server/riot/rateLimiter.test.ts).
import type { RateHostState, RateLimitScope, RateSnapshot } from './types.ts';

/** A snapshot plus when it arrived on this side, so countdowns don't depend on the two clocks agreeing. */
export interface ReceivedRate extends RateSnapshot { receivedAt: number }

export type PaceView =
  | { kind: 'retry'; host: string; scope: RateLimitScope; ms: number }   // Riot answered 429: Retry-After
  | { kind: 'pacing'; host: string; ms: number; paceMs: number }         // queued, next start in ms
  | { kind: 'sending'; host: string; paceMs: number }                    // requests in flight, nothing waiting
  | { kind: 'idle'; ms: number }                                         // up to date, next check in ms
  | { kind: 'quiet' };

export function pickHost(rate: RateSnapshot, host?: string | null): RateHostState | null {
  const name = host ?? rate.active ?? rate.busiest;
  return rate.hosts.find((h) => h.host === name) ?? null;
}

/**
 * Which state the UI shows for one host (default: the active one): "Rate limited by Riot (Retry-After)",
 * "Pacing (next request in X ms)", sending, or the background loop's "Up to date – no new work".
 */
export function paceView(rate: ReceivedRate | null, host?: string | null, now = Date.now()): PaceView {
  if (!rate) return { kind: 'quiet' };
  const elapsed = now - rate.receivedAt;                 // time since the snapshot, on our own clock
  const left = (at: number) => Math.max(0, at - rate.at - elapsed);
  const h = pickHost(rate, host);
  if (h?.retryAfter && left(h.retryAfter.until) > 0) {
    return { kind: 'retry', host: h.host, scope: h.retryAfter.scope, ms: left(h.retryAfter.until) };
  }
  if (h && h.queued > 0 && h.nextAt !== null && left(h.nextAt) > 0) return { kind: 'pacing', host: h.host, ms: left(h.nextAt), paceMs: h.paceMs };
  if (h && (h.queued > 0 || h.inFlight > 0)) return { kind: 'sending', host: h.host, paceMs: h.paceMs };
  if (rate.idleUntil !== null && left(rate.idleUntil) > 0) return { kind: 'idle', ms: left(rate.idleUntil) };
  return { kind: 'quiet' };
}

/** "1 req / 1.3 s", or "45 req/s" for production keys. */
export function fmtPace(ms: number) {
  if (!(ms > 0)) return '—';
  if (ms >= 1000) return `1 req / ${(ms / 1000).toFixed(1)} s`;
  const perSec = 1000 / ms;
  return `${perSec >= 10 ? Math.round(perSec) : perSec.toFixed(1)} req/s`;
}

/** "820 ms", "7 s", "1 min 5 s" */
export function fmtWait(ms: number) {
  if (ms < 1000) return `${Math.max(10, Math.round(ms / 10) * 10)} ms`;
  const s = Math.ceil(ms / 1000);
  return s < 60 ? `${s} s` : `${Math.floor(s / 60)} min ${s % 60} s`;
}

export const SCOPE_LABEL: Record<RateLimitScope, string> = {
  application: 'app limit',
  method: 'method limit',
  service: 'Riot service busy',
};

/** "kr.api.riotgames.com" → "kr" */
export const shortHost = (host: string) => host.split('.')[0];
