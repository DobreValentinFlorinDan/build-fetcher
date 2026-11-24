// The Data tab's evaluation history (0.9.0): every saved report, oldest first, each comparison with its verdict.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { evalHistory } from './evalHistory.ts';

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-history-'));
after(() => fs.rmSync(dir, { recursive: true, force: true }));

const report = (at: number, o: { lift: [number, number, number]; gate: 'passed' | 'failed'; memory?: [number, number, number, 'passed' | 'failed']; scoring?: [number, number, number, 'passed' | 'failed'] }) => ({
  evaluatedAt: at, games: 1000, gate: o.gate, gateReason: o.gate === 'passed' ? 'lift above zero' : 'interval reaches zero',
  lift: { draft: 0, baseline: 0, delta: o.lift[0], low: o.lift[1], high: o.lift[2] },
  memory: o.memory ? {
    grid: [{ decay: 0.75 }], full: { delta: o.memory[0], low: o.memory[1], high: o.memory[2] }, gate: o.memory[3], gateReason: 'memory',
  } : { grid: [], full: { delta: 0, low: NaN, high: NaN }, gate: 'failed', gateReason: 'only 0 games from older patches' },
  ...(o.scoring ? { scoring: { decisions: [{ key: 'keystone' }], adjusted: { delta: o.scoring[0], low: o.scoring[1], high: o.scoring[2] }, gate: o.scoring[3], gateReason: 'scoring' } } : {}),
});
const write = (name: string, body: unknown) => fs.writeFileSync(path.join(dir, name), typeof body === 'string' ? body : JSON.stringify(body));

test('evaluation history: oldest first, each comparison better, unclear or worse, the newest ones only', () => {
  write('eval-2026-10-03-10-00.json', report(3, { lift: [0.01, 0.002, 0.02], gate: 'passed', memory: [-0.01, -0.02, -0.001, 'failed'] }));
  write('eval-auto-2026-10-01-10-00.json', report(1, { lift: [0.005, -0.004, 0.012], gate: 'failed' }));
  write('eval-auto-2026-10-02-10-00.json', report(2, { lift: [0.02, 0.01, 0.03], gate: 'failed', memory: [0.004, -0.002, 0.01, 'failed'], scoring: [0.01, 0.002, 0.02, 'passed'] }));
  write('eval-2026-10-04-10-00.txt', 'the text report');
  write('eval-2026-10-05-10-00.json', '{ half-written');
  write('latest.json', { evaluatedAt: 9, gate: 'passed' });
  const h = evalHistory(dir);
  assert.deepEqual(h.map((e) => [e.at, e.auto, e.file]), [[1, true, 'eval-auto-2026-10-01-10-00.json'], [2, true, 'eval-auto-2026-10-02-10-00.json'], [3, false, 'eval-2026-10-03-10-00.json']]);
  assert.deepEqual(h.map((e) => e.draft.verdict), ['unclear', 'unclear', 'better'], "the draft gate's verdict: passed is better, even an interval above zero isn't without it");
  assert.deepEqual(h.map((e) => e.memory?.verdict ?? null), [null, 'unclear', 'worse'], 'not evaluated (too few older games) is no point at all');
  assert.deepEqual(h.map((e) => e.scoring?.verdict ?? null), [null, 'better', null], 'before 0.9.0: no scoring');
  assert.deepEqual(evalHistory(dir, 2).map((e) => e.at), [2, 3], 'the newest only');
  assert.deepEqual(evalHistory(path.join(dir, 'missing')), []);
  assert.ok(h.every((e) => !e.update && e.fullAt === undefined), 'full evaluations');
});

test('evaluation history: quick updates (0.16.0), yours and automatic, are read and marked with the full evaluation they add to', () => {
  const quick = path.join(dir, 'quick');
  fs.mkdirSync(quick);
  const put = (name: string, body: unknown) => fs.writeFileSync(path.join(quick, name), JSON.stringify(body));
  put('eval-auto-2026-10-21-10-00.json', report(1, { lift: [0.01, 0.002, 0.02], gate: 'passed' }));
  put('eval-auto-update-2026-10-21-12-00.json', { ...report(2, { lift: [0.011, 0.003, 0.02], gate: 'passed' }), update: { fullAt: 1, newGames: 312, reason: '312 new games' } });
  put('eval-update-2026-10-21-14-00.json', { ...report(3, { lift: [0.012, 0.004, 0.021], gate: 'passed' }), update: { fullAt: 1, newGames: 40, reason: '40 new games' } });
  // Named as one, its report from a version that didn't say: still a quick update.
  put('eval-update-2026-10-21-15-00.json', report(4, { lift: [0.012, 0.004, 0.021], gate: 'passed' }));
  put('eval-quick-2026-10-21-16-00.json', report(5, { lift: [0, 0, 0], gate: 'failed' }));
  const h = evalHistory(quick);
  assert.deepEqual(h.map((e) => [e.at, e.auto, e.update, e.fullAt ?? null, e.newGames ?? null]),
    [[1, true, false, null, null], [2, true, true, 1, 312], [3, false, true, 1, 40], [4, false, true, null, null]], 'other names are not reports');
});

test('evaluation history: late-game swaps (0.17.0) are a point where the report compares them, none before', () => {
  const late = path.join(dir, 'swaps');
  fs.mkdirSync(late);
  const put = (name: string, body: unknown) => fs.writeFileSync(path.join(late, name), JSON.stringify(body));
  const swaps = (delta: number, low: number, high: number, gate: 'passed' | 'failed', situations = 900) => ({
    lift: { advice: 0.5 + delta, keep: 0.5, delta, low, high }, situations, games: 400, swapped: 0.2, advised: 0.15, gate, gateReason: `swaps ${gate}`,
  });
  put('eval-2026-10-20-10-00.json', report(1, { lift: [0.01, 0.002, 0.02], gate: 'passed' }));
  put('eval-2026-10-21-10-00.json', { ...report(2, { lift: [0.01, 0.002, 0.02], gate: 'passed' }), swaps: swaps(0.012, 0.003, 0.021, 'passed') });
  put('eval-2026-10-22-10-00.json', { ...report(3, { lift: [0.01, 0.002, 0.02], gate: 'passed' }), swaps: swaps(0.004, -0.003, 0.011, 'failed') });
  put('eval-2026-10-23-10-00.json', { ...report(4, { lift: [0.01, 0.002, 0.02], gate: 'passed' }), swaps: swaps(-0.01, -0.02, -0.002, 'failed') });
  put('eval-2026-10-24-10-00.json', { ...report(5, { lift: [0.01, 0.002, 0.02], gate: 'passed' }), swaps: null });
  put('eval-2026-10-25-10-00.json', { ...report(6, { lift: [0.01, 0.002, 0.02], gate: 'passed' }), swaps: swaps(0, NaN, NaN, 'failed', 0) });
  const h = evalHistory(late);
  assert.deepEqual(h.map((e) => e.swaps?.verdict ?? null), [null, 'better', 'unclear', 'worse', null, null],
    'before 0.17.0, not evaluated, or no late full-inventory visit to compare: no point');
  assert.deepEqual(h[1].swaps, { delta: 0.012, low: 0.003, high: 0.021, verdict: 'better', reason: 'swaps passed' });
  assert.ok(h.every((e) => e.draft.verdict === 'better'), 'the other comparisons as before');
});
