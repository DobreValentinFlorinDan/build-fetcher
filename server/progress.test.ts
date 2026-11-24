// Progress and estimates (0.10.0): the time left from the last run and the action's own pace, the evaluation's steps
// weighed by what they took, and the timings kept next to the settings.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-progress-'));
process.env.IRELIA_CONFIG_DIR = dir;
after(() => fs.rmSync(dir, { recursive: true, force: true }));
const { EVAL_SERIAL_SHARE, expectedMs, lastTiming, Progress, recordTiming, stepFraction, threadMeasure } = await import('./progress.ts');

test('progress: the expected time counts down, then the action\'s own pace takes over as it goes', () => {
  const t0 = 1_000_000;
  const p = new Progress(60_000, t0);
  assert.deepEqual(p.info(0, t0), { fraction: 0, etaMs: 60_000, startedAt: t0, at: t0 });
  // Too early for its pace: the expected time minus what has passed.
  assert.equal(p.info(0.005, t0 + 2000).etaMs, 58_000);
  // A quarter done after 10 s: its pace says 30 s more, the expected time 50 s; halfway between at a quarter.
  assert.equal(p.info(0.25, t0 + 10_000).etaMs, 40_000);
  // From halfway, its pace alone.
  assert.equal(p.info(0.5, t0 + 20_000).etaMs, 20_000);
  // Past the expected time with nothing counted: nothing left by it ("taking longer than estimated").
  assert.equal(new Progress(5000, t0).info(null, t0 + 8000).etaMs, 0);
  // Nothing to go on: no estimate until its pace says something.
  const fresh = new Progress(null, t0);
  assert.equal(fresh.info(null, t0 + 5000).etaMs, null);
  assert.equal(fresh.info(0.1, t0 + 5000).etaMs, 45_000);
  assert.equal(fresh.info(1.5, t0 + 5000).fraction, 1, 'held to 0-1');
});

test('progress: steps weigh what they took last time, else the same', () => {
  // Without weights: 2 of 4 steps done, half of the third.
  assert.equal(stepFraction(3, 4, 0.5), 2.5 / 4);
  // The third step took 70 of 100 s last time: halfway through it is 15 + 35 of 100.
  assert.equal(stepFraction(3, 4, 0.5, [10, 5, 70, 15]), 0.5);
  // Weights for another number of steps (another kind of run) are ignored.
  assert.equal(stepFraction(3, 4, 0.5, [10, 5, 70]), 2.5 / 4);
  assert.equal(stepFraction(1, 4, 0, [10, 5, 70, 15]), 0);
  assert.equal(stepFraction(4, 4, 1, [10, 5, 70, 15]), 1);
});

test('progress: an action\'s time is kept, the newest first, and scales the next estimate to its size', () => {
  // Before any run: the evaluation and the compaction have a default, the others nothing.
  assert.ok(expectedMs('evaluate', 54_000)! > 60_000);
  assert.equal(expectedMs('audit', 1000), null);
  recordTiming('audit', { size: 1000, ms: 30_000, at: 1 });
  recordTiming('audit', { size: 2000, ms: 50_000, at: 2, steps: [1, 2, 3] });
  assert.deepEqual(lastTiming('audit'), { size: 2000, ms: 50_000, at: 2, steps: [1, 2, 3] });
  // Time grows with the size (in proportion for the audit; faster for the evaluation, which compares games pairwise).
  assert.equal(expectedMs('audit', 3000), 75_000);
  recordTiming('evaluate', { size: 10_000, ms: 60_000, at: 3 });
  assert.ok(expectedMs('evaluate', 20_000)! > 120_000);
  // On more threads (0.11.0): the share that runs on one stays, the rest is shared out.
  assert.equal(Math.round(expectedMs('evaluate', 10_000, { threads: 4 })!), Math.round(60_000 * (EVAL_SERIAL_SHARE + (1 - EVAL_SERIAL_SHARE) / 4)));
  assert.equal(expectedMs('evaluate', 10_000, { threads: 1 }), 60_000);
  // A run on four threads says what one would take, and what eight would.
  recordTiming('evaluate', { size: 10_000, ms: 30_000, at: 4, threads: 4, bytesPerGame: 22_000 });
  const one = 30_000 / (EVAL_SERIAL_SHARE + (1 - EVAL_SERIAL_SHARE) / 4);
  assert.equal(Math.round(expectedMs('evaluate', 10_000, { threads: 1 })!), Math.round(one));
  assert.ok(expectedMs('evaluate', 10_000, { threads: 8 })! < 30_000);
  // That measure was taken before the threads' layout had a number (0.11.0: layout 1).
  assert.equal(threadMeasure(1), 22_000);
  assert.equal(threadMeasure(2), null, 'another layout\'s measure says nothing about this one');
  recordTiming('evaluate', { size: 10_000, ms: 25_000, at: 5, threads: 4, bytesPerGame: 10_000, layout: 2 });
  assert.equal(threadMeasure(2), 10_000);
  // A run on one thread (the memory held no second one) measures nothing: the last measure stays.
  recordTiming('evaluate', { size: 10_000, ms: 80_000, at: 6, threads: 1 });
  assert.equal(threadMeasure(2), 10_000);
  // Other actions don't scale with threads.
  assert.equal(expectedMs('audit', 2000, { threads: 8 }), 50_000);
  // A run with nothing done or no time says nothing.
  recordTiming('audit', { size: 0, ms: 10, at: 4 });
  assert.equal(lastTiming('audit')!.at, 2);
  // Runs kept: the newest five.
  for (let i = 0; i < 7; i++) recordTiming('backfill', { size: 100, ms: 1000 + i, at: 10 + i });
  const kept = JSON.parse(fs.readFileSync(path.join(dir, 'timings.json'), 'utf8')) as Record<string, { at: number }[]>;
  assert.deepEqual(kept.backfill.map((t) => t.at), [16, 15, 14, 13, 12]);
});
