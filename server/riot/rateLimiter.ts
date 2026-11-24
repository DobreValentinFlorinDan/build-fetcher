// Riot API request pacing: one RateLimiter per host. Riot counts app limits per routing value, so the
// platform host (kr.api…: league, mastery) and the regional host (asia.api…: match-v5) each get their own.
//
// Up to 0.1.0 requests went out back-to-back until a sliding window was full, then waited for it to drain:
// a development key sent ~90 requests in a few seconds and then stalled for ~2 minutes. Now request
// *starts* are spaced evenly instead:
//
//   spacing = max over every window of  windowMs / floor(limit × safety)     dev key: 120 s / 90 ≈ 1.33 s
//
// Each window is paced with GCRA (virtual scheduling), which also lets a few requests start back-to-back
// after an idle spell, and keeps a sliding log as the hard backstop. Method limits (X-Method-Rate-Limit) get
// their own pacer per endpoint family on the same host; a request has to satisfy both. Waiters form one FIFO
// queue per host and at most `concurrency` requests are in flight, so concurrency overlaps network latency
// but never raises the rate. The total stays what the key allows (90 per 2 minutes for a dev key); only the
// stalls disappear.
import type { RateHostState, RateLimitScope } from '../../shared/types.ts';

export const sleep = (ms: number, signal?: AbortSignal) =>
  new Promise<void>((resolve, reject) => {
    if (signal?.aborted) { reject(signal.reason); return; }
    if (ms <= 0) { resolve(); return; }
    const onAbort = () => { clearTimeout(timer); reject(signal!.reason); };
    const timer = setTimeout(() => { signal?.removeEventListener('abort', onAbort); resolve(); }, ms);
    signal?.addEventListener('abort', onAbort, { once: true });
  });

export interface RateWindow { limit: number; windowMs: number }

export interface PacingOptions {
  /** Share of each window we plan to use. 0.9 leaves room for clock and latency drift between us and Riot. */
  safety: number;
  /** Requests that may start back-to-back after an idle spell (1–3). */
  burst: number;
  /** Requests in flight at once per host (2–3). Start times still follow the pacing. */
  concurrency: number;
  /** Pace multiplier after an application or method 429: 0.8 makes the spacing 25 % longer. */
  slowdown: number;
  /** Time to climb back from one slow-down step to full pace. */
  recoveryMs: number;
}

/** Endpoint families, each with its own method limit on a host. */
export type EndpointFamily = 'league' | 'mastery' | 'matchIds' | 'match' | 'timeline' | 'account';

/**
 * X-Rate-Limit-Type of a 429. `application` and `method` mean we used up a budget; `service` means Riot's
 * backend is busy (Riot sends it, or leaves the header out), which is not about our budget at all.
 */
export type LimitScope = RateLimitScope;

export const limitScope = (header: string | null | undefined): LimitScope =>
  header === 'application' || header === 'method' ? header : 'service';

/** Retry-After is in seconds. */
export const retryAfterMs = (header: string | null | undefined): number | null => {
  const s = Number(header);
  return header != null && header !== '' && Number.isFinite(s) && s >= 0 ? s * 1000 : null;
};

/** "20:1,100:120" → [{limit: 20, windowMs: 1000}, {limit: 100, windowMs: 120000}] */
export function parseLimits(header: string | null | undefined): RateWindow[] {
  if (!header) return [];
  return header.split(',')
    .map((pair) => pair.trim().split(':').map(Number))
    .filter(([limit, s]) => Number.isFinite(limit) && Number.isFinite(s) && limit > 0 && s > 0)
    .map(([limit, s]) => ({ limit, windowMs: s * 1000 }));
}

const MIN_PACE_FACTOR = 0.25;              // repeated 429s never slow a scope below a quarter of its pace
const UNKNOWN_LIMITS_HORIZON_MS = 600_000; // history kept for a scope whose limits haven't arrived yet
const SERVICE_BACKOFF_MAX_MS = 30_000;
const DEFAULT_RETRY_AFTER_MS = 5_000;      // app/method 429 without a Retry-After header (Riot always sends one)

interface Lane {
  limit: number;
  windowMs: number;
  cap: number;       // floor(limit × safety): the budget we plan per window
  burst: number;     // back-to-back starts allowed after idle; cap + burst − 1 never exceeds the limit
  interval: number;  // windowMs / cap: the spacing this window asks for at full pace
  tat: number;       // GCRA theoretical arrival time
}

/** Paces one scope: the app limits of a host, or one endpoint family's method limits on that host. */
export class Pacer {
  private lanes: Lane[] = [];
  private starts: number[] = [];  // request start times, ascending, pruned to the longest window
  private blockedUntil = 0;       // Retry-After of the last 429 on this scope
  private slow = { factor: 1, at: 0 };

  constructor(limits: RateWindow[], private o: Omit<PacingOptions, 'concurrency'>) {
    this.setLimits(limits);
  }

  get limits(): RateWindow[] {
    return this.lanes.map(({ limit, windowMs }) => ({ limit, windowMs }));
  }

  /** Applies limits from a response header. Returns true when they changed; the start history is kept. */
  setLimits(limits: RateWindow[]): boolean {
    if (!limits.length) return false;
    const same = limits.length === this.lanes.length
      && limits.every((l, i) => l.limit === this.lanes[i].limit && l.windowMs === this.lanes[i].windowMs);
    if (same) return false;
    this.lanes = limits.map(({ limit, windowMs }) => {
      const cap = Math.max(1, Math.floor(limit * this.o.safety));
      const burst = Math.max(1, Math.min(Math.floor(this.o.burst), limit - cap + 1));
      const lane: Lane = { limit, windowMs, cap, burst, interval: windowMs / cap, tat: 0 };
      // Replay what already went out, so new or refreshed limits pick up the current schedule.
      for (const t of this.starts) lane.tat = Math.max(lane.tat, t) + lane.interval;
      return lane;
    });
    return true;
  }

  /**
   * Pace multiplier: drops after a 429 and, once its Retry-After is over, climbs back linearly by one
   * slow-down step per `recoveryMs`.
   */
  factor(now: number) {
    const climbed = this.slow.factor + (Math.max(0, now - this.slow.at) * (1 - this.o.slowdown)) / this.o.recoveryMs;
    return Math.min(1, climbed);
  }

  /** Spacing between request starts right now (0 while the limits are unknown). */
  spacing(now: number) {
    const base = this.lanes.reduce((max, lane) => Math.max(max, lane.interval), 0);
    return base / this.factor(now);
  }

  /** Earliest start for the next request: Retry-After, then GCRA per window, then the sliding-log backstop. */
  earliest(now: number): number {
    this.prune(now);
    const f = this.factor(now);
    let t = Math.max(now, this.blockedUntil);
    for (const lane of this.lanes) {
      const interval = lane.interval / f;
      t = Math.max(t, lane.tat - (lane.burst - 1) * interval);
      // Backstop: never more than GCRA itself can admit into one window (cap + burst − 1 ≤ limit).
      const room = lane.cap + lane.burst - 1;
      if (this.inWindow(lane.windowMs, now) >= room) t = Math.max(t, this.starts[this.starts.length - room] + lane.windowMs);
    }
    return t;
  }

  /** Records a request that starts now. */
  commit(at: number) {
    const t = Math.max(at, this.starts[this.starts.length - 1] ?? at);
    const f = this.factor(t);
    for (const lane of this.lanes) lane.tat = Math.max(lane.tat, t) + lane.interval / f;
    this.starts.push(t);
  }

  /** A 429 on this scope: wait out Retry-After and, unless told otherwise, slow the pace down for a while. */
  penalize(now: number, retryAfter: number, slowDown = true) {
    this.blockedUntil = Math.max(this.blockedUntil, now + retryAfter);
    if (slowDown) this.slow = { factor: Math.max(MIN_PACE_FACTOR, this.factor(now) * this.o.slowdown), at: this.blockedUntil };
    this.resumeAt(this.blockedUntil);
  }

  /** After a forced pause, carry on at the normal spacing from `t`: no catch-up burst. */
  resumeAt(t: number) {
    const f = this.factor(t);
    for (const lane of this.lanes) lane.tat = Math.max(lane.tat, t + ((lane.burst - 1) * lane.interval) / f);
  }

  /** When the current Retry-After ends, or 0 when there is none. */
  retryUntil(now: number) {
    return this.blockedUntil > now ? this.blockedUntil : 0;
  }

  usage(now: number) {
    this.prune(now);
    return this.lanes.map((lane) => ({ used: this.inWindow(lane.windowMs, now), limit: lane.limit, windowSec: lane.windowMs / 1000 }));
  }

  /** Starts inside (now − windowMs, now]. */
  private inWindow(windowMs: number, now: number) {
    const edge = now - windowMs;
    let lo = 0, hi = this.starts.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (this.starts[mid] > edge) hi = mid; else lo = mid + 1;
    }
    return this.starts.length - lo;
  }

  private prune(now: number) {
    const horizon = this.lanes.length ? Math.max(...this.lanes.map((lane) => lane.windowMs)) : UNKNOWN_LIMITS_HORIZON_MS;
    const expired = this.starts.length - this.inWindow(horizon, now);
    if (expired > 0) this.starts.splice(0, expired);
  }
}

/** Held while a request is in flight: call `done()` once its response has been read. */
export interface Permit { readonly startedAt: number; done(): void }

interface Waiter {
  family: EndpointFamily;
  resolve: (permit: Permit) => void;
  reject: (reason: unknown) => void;
  signal?: AbortSignal;
  onAbort?: () => void;
}

/** All Riot traffic to one host: app pacer, method pacers per endpoint family, service back-off, FIFO queue. */
export class RateLimiter {
  readonly app: Pacer;
  private methods = new Map<EndpointFamily, Pacer>();
  private queue: Waiter[] = [];
  private timer: ReturnType<typeof setTimeout> | null = null;
  private inFlight = 0;
  private serviceUntil = 0;
  private serviceStrikes = 0;
  private lastFamily: EndpointFamily | null = null;
  private lastStartAt = 0;

  constructor(readonly host: string, limits: RateWindow[], private o: PacingOptions) {
    this.app = new Pacer(limits, o);
  }

  /** Waits for this request's turn and start slot. Rejects with the signal's reason if aborted while waiting. */
  acquire(family: EndpointFamily, signal?: AbortSignal): Promise<Permit> {
    return new Promise<Permit>((resolve, reject) => {
      if (signal?.aborted) { reject(signal.reason); return; }
      const waiter: Waiter = { family, resolve, reject, signal };
      if (signal) {
        waiter.onAbort = () => {
          const i = this.queue.indexOf(waiter);
          if (i < 0) return;
          this.queue.splice(i, 1);
          reject(signal.reason);
          if (i === 0) this.pump();
        };
        signal.addEventListener('abort', waiter.onAbort, { once: true });
      }
      this.queue.push(waiter);
      if (this.queue.length === 1) this.pump();
    });
  }

  /** Limits from a response: X-App-Rate-Limit and X-Method-Rate-Limit. */
  updateLimits(family: EndpointFamily, appHeader: string | null, methodHeader: string | null) {
    const app = this.app.setLimits(parseLimits(appHeader));
    const method = this.method(family).setLimits(parseLimits(methodHeader));
    if (app || method) this.pump();
  }

  /**
   * Riot answered 429. Application or method scope: wait out Retry-After on that scope and slow its pace
   * down for a while. Service scope: Riot's backend is busy, not our budget, so only this host backs off
   * (Retry-After, else 1 s, 2 s, 4 s … up to 30 s) and the pace stays as it is.
   * Returns how long the next request of this family waits.
   */
  rateLimited(family: EndpointFamily, scope: LimitScope, retryAfter: number | null): number {
    const now = Date.now();
    if (scope === 'service') {
      const wait = retryAfter ?? Math.min(SERVICE_BACKOFF_MAX_MS, 1000 * 2 ** this.serviceStrikes);
      this.serviceStrikes++;
      this.serviceUntil = Math.max(this.serviceUntil, now + wait);
      for (const pacer of [this.app, ...this.methods.values()]) pacer.resumeAt(this.serviceUntil);
    } else {
      (scope === 'application' ? this.app : this.method(family)).penalize(now, retryAfter ?? DEFAULT_RETRY_AFTER_MS);
    }
    this.pump();
    return Math.max(0, this.blockedUntil(family, now) - now);
  }

  /** A normal answer (2xx or 404): the service back-off starts from scratch next time. */
  succeeded() {
    this.serviceStrikes = 0;
  }

  state(now: number): RateHostState {
    const head = this.queue[0];
    const next = head ? this.earliest(head, now) : null;
    const family = head?.family ?? this.lastFamily;
    const method = family ? this.methods.get(family) : undefined;
    const until = family ? this.blockedUntil(family, now) : Math.max(this.serviceUntil, this.app.retryUntil(now));
    return {
      host: this.host,
      windows: this.app.usage(now),
      paceMs: Math.max(this.app.spacing(now), method?.spacing(now) ?? 0),
      nextAt: next === null || next === Infinity ? null : next,
      queued: this.queue.length,
      inFlight: this.inFlight,
      retryAfter: until > now ? { until, scope: this.blockingScope(family, until, now) } : null,
      lastStartAt: this.lastStartAt,
    };
  }

  /** Window usage of the app limits. */
  usage(now = Date.now()) {
    return this.app.usage(now);
  }

  private method(family: EndpointFamily) {
    let pacer = this.methods.get(family);
    if (!pacer) {
      pacer = new Pacer([], this.o);   // limits arrive with the first response; starts are recorded until then
      this.methods.set(family, pacer);
    }
    return pacer;
  }

  private blockedUntil(family: EndpointFamily, now: number) {
    return Math.max(this.serviceUntil, this.app.retryUntil(now), this.methods.get(family)?.retryUntil(now) ?? 0);
  }

  private blockingScope(family: EndpointFamily | null, until: number, now: number): LimitScope {
    if (until === this.serviceUntil) return 'service';
    if (until === this.app.retryUntil(now)) return 'application';
    return family && this.methods.get(family)?.retryUntil(now) === until ? 'method' : 'service';
  }

  private earliest(waiter: Waiter, now: number) {
    if (this.inFlight >= this.o.concurrency) return Infinity;   // a response has to come back first
    return Math.max(now, this.serviceUntil, this.app.earliest(now), this.method(waiter.family).earliest(now));
  }

  /** Grants the head of the queue its slot, or sleeps until it is due. Re-run on every state change. */
  private pump() {
    if (this.timer) { clearTimeout(this.timer); this.timer = null; }
    for (;;) {
      const head = this.queue[0];
      if (!head) return;
      const now = Date.now();
      const at = this.earliest(head, now);
      if (at === Infinity) return;            // Permit.done() pumps again
      const wait = Math.ceil(at - now);
      if (wait > 0) {
        this.timer = setTimeout(() => { this.timer = null; this.pump(); }, wait);
        return;
      }
      this.queue.shift();
      if (head.onAbort) head.signal?.removeEventListener('abort', head.onAbort);
      this.grant(head, now);
    }
  }

  private grant(waiter: Waiter, now: number) {
    this.app.commit(now);
    this.method(waiter.family).commit(now);
    this.inFlight++;
    this.lastFamily = waiter.family;
    this.lastStartAt = now;
    let released = false;
    waiter.resolve({
      startedAt: now,
      done: () => {
        if (released) return;
        released = true;
        this.inFlight--;
        this.pump();
      },
    });
  }
}
