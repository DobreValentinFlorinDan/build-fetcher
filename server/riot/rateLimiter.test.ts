import assert from 'node:assert/strict';
import { afterEach, beforeEach, mock, test } from 'node:test';
import { fmtPace, fmtWait, paceView } from '../../shared/rateView.ts';
import type { RateSnapshot } from '../../shared/types.ts';
import { RiotClient } from './client.ts';
import type { DiskCache } from './diskCache.ts';
import {
  limitScope, parseLimits, RateLimiter, retryAfterMs, sleep, type EndpointFamily, type PacingOptions, type RateWindow,
} from './rateLimiter.ts';

// Fake setTimeout + Date: every wait below is simulated, nothing sleeps for real.
beforeEach(() => mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 0 }));
afterEach(() => mock.timers.reset());

const DEV_KEY = parseLimits('20:1,100:120');
const OPTS: PacingOptions = { safety: 0.9, burst: 3, concurrency: 3, slowdown: 0.8, recoveryMs: 60_000 };
const SPACING = 120_000 / 90;   // 100 per 120 s at 90 % → one start every 1333.3 ms
const STEP = 5;                 // fake-clock resolution of the simulations

/** One real macrotask turn (setImmediate isn't faked), so pending promise continuations run. */
const settle = () => new Promise<void>((resolve) => setImmediate(resolve));

/** Moves the fake clock forward in small steps, letting async code react after each one. */
async function advance(ms: number) {
  for (let t = 0; t < ms; t += STEP) {
    mock.timers.tick(Math.min(STEP, ms - t));
    await settle();
  }
}

interface Run { starts: number[]; maxInFlight: number }

/** `workers` loops that each take a slot, keep a request in flight for `latencyMs` and release it. */
async function simulate(limiter: RateLimiter, o: { workers: number; latencyMs: number; durationMs: number; family?: EndpointFamily }): Promise<Run> {
  const run: Run = { starts: [], maxInFlight: 0 };
  const stop = new AbortController();
  let inFlight = 0;
  const worker = async () => {
    while (!stop.signal.aborted) {
      const permit = await limiter.acquire(o.family ?? 'match', stop.signal).catch(() => null);
      if (!permit) return;
      run.starts.push(permit.startedAt);
      run.maxInFlight = Math.max(run.maxInFlight, ++inFlight);
      await sleep(o.latencyMs);
      inFlight--;
      permit.done();
    }
  };
  const loops = Array.from({ length: o.workers }, worker);
  await advance(o.durationMs);
  stop.abort(new Error('done'));
  await advance(o.latencyMs + STEP);
  await Promise.all(loops);
  return run;
}

/** Most starts that fell into any window of `windowMs` (half-open, like Riot's). */
function busiestWindow(starts: number[], windowMs: number) {
  let max = 0;
  for (let i = 0, lo = 0; i < starts.length; i++) {
    while (starts[i] - starts[lo] >= windowMs) lo++;
    max = Math.max(max, i - lo + 1);
  }
  return max;
}

const gaps = (starts: number[]) => starts.slice(1).map((t, i) => t - starts[i]);
const median = (xs: number[]) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

/** Takes one slot as soon as it is granted and records when that was. */
async function take(limiter: RateLimiter, family: EndpointFamily = 'match', signal?: AbortSignal) {
  const permit = await limiter.acquire(family, signal);
  permit.done();
  return permit.startedAt;
}

/** Starts of `n` requests issued one after another (no concurrency), with the fake clock running. */
async function sequence(limiter: RateLimiter, n: number, family: EndpointFamily = 'match') {
  const starts: number[] = [];
  const done = (async () => { for (let i = 0; i < n; i++) starts.push(await take(limiter, family)); })();
  while (starts.length < n) await advance(100);
  await done;
  return starts;
}

test('header parsing: limits, Retry-After and the 429 scope', () => {
  assert.deepEqual(parseLimits('20:1,100:120'), [{ limit: 20, windowMs: 1000 }, { limit: 100, windowMs: 120_000 }]);
  assert.deepEqual(parseLimits(' 500:10 , bogus, 0:5 '), [{ limit: 500, windowMs: 10_000 }]);
  assert.deepEqual(parseLimits(null), []);
  assert.equal(retryAfterMs('7'), 7000);
  assert.equal(retryAfterMs(null), null);
  assert.equal(limitScope('application'), 'application');
  assert.equal(limitScope('method'), 'method');
  assert.equal(limitScope('service'), 'service');
  assert.equal(limitScope(null), 'service');   // Riot leaves the header out when its backend is busy
});

test('never exceeds any window over a simulated 10-minute run, and never stalls', async () => {
  const limiter = new RateLimiter('asia.api.riotgames.com', DEV_KEY, OPTS);
  const { starts, maxInFlight } = await simulate(limiter, { workers: 3, latencyMs: 150, durationMs: 600_000 });

  for (const w of DEV_KEY) {
    const cap = Math.floor(w.limit * OPTS.safety);
    const most = busiestWindow(starts, w.windowMs);
    assert.ok(most <= w.limit, `${most} requests in a ${w.windowMs / 1000}s window, limit ${w.limit}`);
    assert.ok(most <= cap + OPTS.burst - 1, `${most} requests in a ${w.windowMs / 1000}s window, planned ${cap} + burst`);
  }
  // Same budget as before (90 per 2 minutes, plus the initial burst), but spread out.
  assert.ok(starts.length >= 445 && starts.length <= 455, `${starts.length} requests in 10 minutes`);
  assert.ok(Math.max(...gaps(starts)) <= SPACING + STEP, `longest gap ${Math.max(...gaps(starts))} ms`);
  assert.ok(maxInFlight <= OPTS.concurrency);
});

test('spacing converges to windowMs / floor(limit × safety) after a small burst', async () => {
  const limiter = new RateLimiter('kr.api.riotgames.com', DEV_KEY, OPTS);
  const { starts } = await simulate(limiter, { workers: 3, latencyMs: 80, durationMs: 120_000 });

  assert.deepEqual(starts.slice(0, OPTS.burst), [0, 0, 0], 'up to `burst` requests may start at once after idle');
  assert.ok(starts[OPTS.burst] >= SPACING - STEP, 'then the pacing takes over');
  const steady = gaps(starts.slice(OPTS.burst));
  assert.ok(Math.abs(median(steady) - SPACING) <= STEP, `median gap ${median(steady)} ms`);
  const mean = steady.reduce((s, g) => s + g, 0) / steady.length;
  assert.ok(Math.abs(mean - SPACING) < 1, `mean gap ${mean} ms`);
  assert.ok(Math.abs(limiter.state(Date.now()).paceMs - SPACING) < 0.01);
});

test('concurrency overlaps latency but never raises the rate', async () => {
  const single = await simulate(new RateLimiter('h', DEV_KEY, { ...OPTS, concurrency: 1 }), { workers: 1, latencyMs: 400, durationMs: 300_000 });
  const three = await simulate(new RateLimiter('h', DEV_KEY, OPTS), { workers: 6, latencyMs: 400, durationMs: 300_000 });
  assert.ok(Math.abs(single.starts.length - three.starts.length) <= OPTS.burst, `${single.starts.length} vs ${three.starts.length}`);
  assert.equal(three.maxInFlight, 3, 'six callers, but at most three requests in flight');

  // When latency is longer than the spacing (production keys), concurrency is what keeps the pipe full.
  const fast: RateWindow[] = parseLimits('500:10,30000:600');   // 450 per 10 s planned → 22.2 ms apart
  const slow1 = await simulate(new RateLimiter('h', fast, { ...OPTS, concurrency: 1 }), { workers: 1, latencyMs: 60, durationMs: 10_000 });
  const slow3 = await simulate(new RateLimiter('h', fast, OPTS), { workers: 3, latencyMs: 60, durationMs: 10_000 });
  assert.ok(slow3.starts.length > 2.5 * slow1.starts.length, `${slow1.starts.length} → ${slow3.starts.length}`);
  assert.ok(busiestWindow(slow3.starts, 10_000) <= 452);
});

test('an application 429 waits out Retry-After and slows the pace, then it recovers', async () => {
  const limiter = new RateLimiter('asia.api.riotgames.com', DEV_KEY, OPTS);
  await sequence(limiter, 3);

  const wait = limiter.rateLimited('match', 'application', 10_000);
  assert.equal(wait, 10_000);
  const state = limiter.state(Date.now());
  assert.equal(state.retryAfter?.scope, 'application');
  assert.ok(Math.abs(state.paceMs - SPACING / 0.8) < 0.01, `pace after a 429: ${state.paceMs} ms`);

  const t429 = Date.now();
  const [first, second] = await sequence(limiter, 2);
  assert.ok(first >= t429 + 10_000, 'nothing goes out before Retry-After has passed');
  assert.ok(second - first >= SPACING / 0.8 - STEP, `slower spacing right after: ${second - first} ms`);

  await advance(60_000);   // one recovery period without another 429
  assert.ok(Math.abs(limiter.state(Date.now()).paceMs - SPACING) < 0.01, 'back to full pace');
  assert.equal(limiter.state(Date.now()).retryAfter, null);
});

test('a service 429 backs off only that host and keeps the pace', async () => {
  const kr = new RateLimiter('kr.api.riotgames.com', DEV_KEY, OPTS);
  const asia = new RateLimiter('asia.api.riotgames.com', DEV_KEY, OPTS);
  await sequence(kr, 3);
  await sequence(asia, 3);

  const t = Date.now();
  assert.equal(kr.rateLimited('mastery', 'service', null), 1000, 'no Retry-After: back off 1 s …');
  assert.equal(kr.state(t).retryAfter?.scope, 'service');
  assert.ok(Math.abs(kr.state(t).paceMs - SPACING) < 0.01, 'service trouble is not our budget: pace unchanged');
  assert.ok(Math.abs(kr.app.factor(t) - 1) < 1e-9);
  assert.equal(asia.state(t).retryAfter, null, 'the other host is not affected');

  const [krNext] = await sequence(kr, 1, 'mastery');
  assert.ok(krNext >= t + 1000);
  assert.equal(kr.rateLimited('mastery', 'service', null), 2000, '… then 2 s on the next strike');
  await sequence(kr, 1, 'mastery');
  kr.succeeded();
  assert.equal(kr.rateLimited('mastery', 'service', null), 1000, 'a normal answer resets the back-off');
  assert.equal(kr.rateLimited('mastery', 'service', 7000), 7000, 'Retry-After wins when Riot sends one');

  // Contrast: an application 429 does slow the pace down.
  asia.rateLimited('match', 'application', 1000);
  assert.ok(asia.state(Date.now()).paceMs > SPACING * 1.2);
});

test('method limits pace their endpoint family on top of the app limits', async () => {
  const limiter = new RateLimiter('kr.api.riotgames.com', DEV_KEY, OPTS);
  await sequence(limiter, 1, 'league');
  // league-v4 says 5 per 10 s: plan 4 → one every 2.5 s, burst capped at 2 so 4 + 2 − 1 never exceeds 5.
  limiter.updateLimits('league', '20:1,100:120', '5:10');
  const league = await sequence(limiter, 12, 'league');
  assert.ok(median(gaps(league)) >= 2500 - STEP, `league spacing ${median(gaps(league))} ms`);
  assert.ok(busiestWindow(league, 10_000) <= 5);
  assert.ok(Math.abs(limiter.state(Date.now()).paceMs - 2500) < 0.01, 'reported pace is the slower method pace');

  // Other families on the same host still go at the app pace.
  await advance(30_000);
  const mastery = await sequence(limiter, 8, 'mastery');
  assert.ok(Math.abs(median(gaps(mastery.slice(OPTS.burst))) - SPACING) <= STEP, `mastery spacing ${median(gaps(mastery))} ms`);

  // A method 429 blocks that family only.
  const t = Date.now();
  limiter.rateLimited('league', 'method', 20_000);
  const [m] = await sequence(limiter, 1, 'mastery');
  assert.ok(m < t + 20_000, 'mastery is not held back by the league Retry-After');
  const [l] = await sequence(limiter, 1, 'league');
  assert.ok(l >= t + 20_000, 'league waits for its Retry-After');
});

test('abort works mid-wait and the slot goes to the next request', async () => {
  const limiter = new RateLimiter('asia.api.riotgames.com', DEV_KEY, OPTS);
  await sequence(limiter, 3);   // burst used up at t = 0: the next slot is at ~1333 ms

  const cancelled = new AbortController();
  const a = limiter.acquire('match', cancelled.signal);
  const b = limiter.acquire('match');
  await advance(500);
  cancelled.abort(new Error('scan cancelled'));
  await assert.rejects(a, /scan cancelled/);
  await advance(1000);
  const permit = await b;
  permit.done();
  assert.ok(permit.startedAt <= SPACING + STEP, `the aborted request did not use up a slot (b started at ${permit.startedAt})`);

  // Also while waiting out a Retry-After.
  limiter.rateLimited('match', 'application', 30_000);
  const late = new AbortController();
  const c = limiter.acquire('match', late.signal);
  await advance(2000);
  late.abort(new Error('stop'));
  await assert.rejects(c, /stop/);
  assert.equal(limiter.state(Date.now()).queued, 0);

  // An already-aborted signal never queues.
  await assert.rejects(limiter.acquire('match', AbortSignal.abort(new Error('early'))), /early/);
});

test('refreshed limits keep the schedule; new limits replay what already went out', async () => {
  const limiter = new RateLimiter('kr.api.riotgames.com', DEV_KEY, OPTS);
  const before = await sequence(limiter, 5);
  limiter.updateLimits('match', '20:1,100:120', null);   // the same header on every response: no reset
  const [next] = await sequence(limiter, 1);
  assert.ok(next - before[4] >= SPACING - STEP, 'an unchanged header does not hand out a fresh burst');

  limiter.updateLimits('match', '10:1,50:120', null);   // a different key: 45 per 120 s → 2.67 s apart
  assert.ok(Math.abs(limiter.state(Date.now()).paceMs - 120_000 / 45) < 0.01);
  const after = await sequence(limiter, 3);
  assert.ok(Math.min(...gaps(after)) >= 120_000 / 45 - STEP);
});

// ---- the client end to end, with Riot's real headers (fetch stubbed, no cache) ---------------------------

const noCache = { hits: 0, get: async () => undefined, set: async () => {} } as unknown as DiskCache;

/** Stubs fetch with queued responses and records when each request went out. */
function stubFetch(responses: (() => Response)[]) {
  const sent: { url: string; at: number }[] = [];
  const original = globalThis.fetch;
  globalThis.fetch = (async (url: string | URL | Request) => {
    sent.push({ url: String(url), at: Date.now() });
    const next = responses.shift();
    assert.ok(next, `unexpected request ${String(url)}`);
    return next();
  }) as typeof fetch;
  return { sent, restore: () => { globalThis.fetch = original; } };
}

const ok = (body: unknown, method = '2000:10') => () => new Response(JSON.stringify(body), {
  status: 200, headers: { 'x-app-rate-limit': '20:1,100:120', 'x-method-rate-limit': method },
});
const tooMany = (headers: Record<string, string>) => () => new Response('', { status: 429, headers });

/** Runs a client call to completion while the fake clock moves. */
async function settleCall<T>(call: Promise<T>) {
  let done = false;
  const result = call.finally(() => { done = true; });
  result.catch(() => {});   // the caller sees the rejection; don't report it as unhandled meanwhile
  while (!done) await advance(50);
  return result;
}

test('client: an application 429 slows its host, a service 429 only backs off', async () => {
  const fetch = stubFetch([
    tooMany({ 'x-rate-limit-type': 'application', 'retry-after': '3' }), ok({ metadata: { matchId: 'KR_1' } }),
    tooMany({}), ok([{ championId: 39, championPoints: 400_000 }], '20000:10'),
  ]);
  try {
    const riot = new RiotClient('RGAPI-test', noCache, DEV_KEY, OPTS);
    const match = await settleCall(riot.match('KR', 'KR_1'));
    assert.deepEqual(match, { metadata: { matchId: 'KR_1' } });
    assert.ok(fetch.sent[1].at - fetch.sent[0].at >= 3000, 'retried after Retry-After');
    const asia = () => riot.rateState().hosts.find((h) => h.host === 'asia.api.riotgames.com')!;
    assert.ok(asia().paceMs > SPACING * 1.2, 'application 429: the regional host slows down');

    const top = await settleCall(riot.topMasteries('KR', 'puuid-1', 3));
    assert.equal(top[0]?.championPoints, 400_000);
    assert.ok(fetch.sent[3].at - fetch.sent[2].at >= 1000, 'service 429 without Retry-After: 1 s back-off');
    const kr = riot.rateState().hosts.find((h) => h.host === 'kr.api.riotgames.com')!;
    assert.ok(Math.abs(kr.paceMs - SPACING) < 0.01, 'service 429: pace unchanged');
    assert.equal(riot.requests, 4);
  } finally {
    fetch.restore();
  }
});

test('client: an expired key stops with RiotAuthError, no retries', async () => {
  const fetch = stubFetch([() => new Response('', { status: 403 })]);
  try {
    const riot = new RiotClient('RGAPI-expired', noCache, DEV_KEY, OPTS);
    await assert.rejects(settleCall(riot.matchIds('KR', 'p', 420, 20)), /rejected the key/);
    assert.equal(fetch.sent.length, 1);
  } finally {
    fetch.restore();
  }
});

test('client: a dropped connection or a timed-out request is tried again, after a back-off', async () => {
  const fetch = stubFetch([
    () => { throw new TypeError('fetch failed'); },
    () => { throw new DOMException('The operation was aborted due to timeout', 'TimeoutError'); },
    ok({ metadata: { matchId: 'KR_2' } }),
  ]);
  try {
    const riot = new RiotClient('RGAPI-test', noCache, DEV_KEY, OPTS);
    assert.deepEqual(await settleCall(riot.match('KR', 'KR_2')), { metadata: { matchId: 'KR_2' } });
    assert.equal(fetch.sent.length, 3);
    assert.ok(fetch.sent[1].at - fetch.sent[0].at >= 1000 && fetch.sent[2].at - fetch.sent[1].at >= 2000, 'backs off 1 s, then 2 s');
  } finally {
    fetch.restore();
  }
});

test('client: a request that never answers is abandoned at the time limit; a stop by the caller is not retried', async () => {
  const original = globalThis.fetch;
  let calls = 0;
  // The first request hangs until its signal aborts (a connection that died while the computer slept); the second answers.
  globalThis.fetch = (async (_url: string | URL | Request, init?: RequestInit) => {
    calls++;
    if (calls === 1) return new Promise<Response>((_, reject) => init!.signal!.addEventListener('abort', () => reject(init!.signal!.reason)));
    return ok({ metadata: { matchId: 'KR_3' } })();
  }) as typeof fetch;
  try {
    const riot = new RiotClient('RGAPI-test', noCache, DEV_KEY, OPTS);
    riot.requestTimeoutMs = 20;   // a real timer: AbortSignal.timeout isn't on the fake clock
    assert.deepEqual(await settleCall(riot.match('KR', 'KR_3')), { metadata: { matchId: 'KR_3' } });
    assert.equal(calls, 2);

    calls = 0;
    const stop = new AbortController();
    const call = riot.match('KR', 'KR_4', stop.signal);
    call.catch(() => {});
    while (calls === 0) await advance(50);   // sent (it hangs), then stopped by the caller
    stop.abort(new Error('stopped'));
    await assert.rejects(settleCall(call), /stopped/);
    assert.equal(calls, 1, 'not tried again');
  } finally {
    globalThis.fetch = original;
  }
});

// ---- what the dashboard shows for a snapshot -------------------------------------------------------------

test('the UI tells pacing, Retry-After and up to date apart, and counts down on its own clock', async () => {
  const limiter = new RateLimiter('asia.api.riotgames.com', DEV_KEY, OPTS);
  const snap = (idleUntil: number | null = null): RateSnapshot => ({
    at: Date.now(), hosts: [limiter.state(Date.now())], busiest: limiter.host, active: limiter.host, defaultPaceMs: SPACING, idleUntil,
  });
  const seen = (s: RateSnapshot, receivedAt = s.at, now = Date.now()) => paceView({ ...s, receivedAt }, null, now);

  await sequence(limiter, 3);                    // burst used up: the next request has to wait for its slot
  const waiting = limiter.acquire('match');
  const pacing = snap();
  const nextIn = pacing.hosts[0].nextAt! - pacing.at;
  assert.ok(nextIn > 0 && nextIn <= SPACING + 1, `next slot in ${nextIn} ms`);
  let view = seen(pacing);
  assert.deepEqual(view, { kind: 'pacing', host: limiter.host, ms: nextIn, paceMs: SPACING });
  assert.equal(fmtPace(SPACING), '1 req / 1.3 s');
  view = seen(pacing, pacing.at, pacing.at + 400);  // 400 ms later on the dashboard's clock, no new snapshot
  assert.ok(view.kind === 'pacing' && Math.abs(view.ms - (nextIn - 400)) < 1e-6);
  assert.ok(seen(pacing, 5_000_000, 5_000_000 + 400).kind === 'pacing', 'skewed clocks: only the elapsed time counts');
  (await settleCall(waiting)).done();

  limiter.rateLimited('match', 'application', 7000);
  view = seen(snap());
  assert.deepEqual(view.kind === 'retry' && [view.scope, view.ms], ['application', 7000]);
  assert.equal(fmtWait(7000), '7 s');

  await advance(8000);
  view = seen(snap(Date.now() + 60_000));         // loop sleeping: nothing queued, nothing in flight
  assert.deepEqual(view, { kind: 'idle', ms: 60_000 });
  assert.equal(fmtWait(42_000), '42 s');
  assert.equal(seen(snap()).kind, 'quiet');
});

test('client: 5xx is retried with back-off; a 429 on the last attempt still holds the host back', async () => {
  const fetch = stubFetch([
    () => new Response('', { status: 503 }), ok(['KR_1', 'KR_2']),
    ...Array.from({ length: 5 }, () => tooMany({ 'x-rate-limit-type': 'application', 'retry-after': '2' })),
  ]);
  try {
    const riot = new RiotClient('RGAPI-test', noCache, DEV_KEY, OPTS);
    assert.deepEqual(await settleCall(riot.matchIds('KR', 'p', 420, 20)), ['KR_1', 'KR_2']);
    assert.ok(fetch.sent[1].at - fetch.sent[0].at >= 1000, '503: retried after 1 s');

    await assert.rejects(settleCall(riot.match('KR', 'KR_9')), /429/);
    assert.equal(fetch.sent.length, 7, 'five attempts, then it gives up');
    const asia = riot.rateState().hosts.find((h) => h.host === 'asia.api.riotgames.com')!;
    assert.equal(asia.retryAfter?.scope, 'application', 'the last 429 is still honoured by later requests');
  } finally {
    fetch.restore();
  }
});
