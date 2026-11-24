// Use GPU (0.13.0, server/gpu/): what CI can check without an NVIDIA GPU. The kernel itself was checked against
// Electron's processor numbers with a stand-in driver that runs it on the processor (docs/releases/0.13.0.md), and
// the app checks it on the GPU before each evaluation.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { contextsFrom } from '../model/context.ts';
import { flatSimilarities, rowSimilarity, setSimilarityAccelerator, similarities } from '../model/draft.ts';

// The log goes to a folder of its own, and no driver is found.
process.env.IRELIA_CONFIG_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-gpu-'));
process.env.IRELIA_CUDA_LIB = path.join(process.env.IRELIA_CONFIG_DIR, 'no-such-driver.so');
const { startGpu } = await import('./accelerate.ts');
const { readGpuLog } = await import('./log.ts');

const zDraft = contextsFrom(Array.from({ length: 300 }, (_, i) => Array.from({ length: 6 }, (_, c) => ((i + c) % 11 === 0 ? NaN : Math.sin(i * 0.7 + c)))));
const zq = [0.3, NaN, -0.2, 1.1, 0, 0.5];

test('gpu: without an NVIDIA driver, no session, the reason in the log, and the processor as before', async () => {
  assert.equal(await startGpu({ thread: 'thread 1' }), null);
  assert.match(readGpuLog(), /thread 1: No NVIDIA driver: .* could not be loaded .* Evaluate builds runs on the processor\./);
  const before = similarities(zDraft, zq, 0.7).slice();
  assert.ok(before.every((v) => v > 0 && v <= 1));
});

test('gpu: similarities takes the accelerator\'s numbers, or works them out itself when it declines', () => {
  const cpu = similarities(zDraft, zq, 0.7).slice();
  let calls = 0;
  setSimilarityAccelerator((stored, rows, d, q, den, out) => {
    calls++;
    assert.equal(rows, zDraft.rows);
    assert.equal(d, zq.length);
    assert.equal(den, 2 * 0.7 * 0.7);
    for (let i = 0; i < rows; i++) out[i] = rowSimilarity(stored.flat, d, q, den, i);
    return calls === 1;
  });
  try {
    assert.deepEqual(similarities(zDraft, zq, 0.7).slice(), cpu, 'the accelerator\'s (game by game, the same numbers)');
    assert.deepEqual(similarities(zDraft, zq, 0.7).slice(), cpu, 'declined: the processor\'s');
    assert.equal(calls, 2);
    // No bandwidth (h = ∞): every game alike, never the accelerator.
    similarities(zDraft, zq, Infinity);
    assert.equal(calls, 2);
  } finally { setSimilarityAccelerator(null); }
});

test('gpu: one game at a time gives what the loops give, with and without unknown dimensions', () => {
  for (const nan of [false, true]) {
    const d = 5, rows = 200;
    const flat = Float64Array.from({ length: rows * d }, (_, i) => (nan && i % 13 === 0 ? NaN : Math.cos(i * 1.3)));
    for (const q of [Float64Array.from([0.1, 0.2, -0.3, 0.4, 0]), Float64Array.from([0.1, NaN, -0.3, NaN, 0]), Float64Array.from([NaN, NaN, NaN, NaN, NaN])]) {
      const out = flatSimilarities({ flat, anyNaN: nan }, rows, d, q, 0.5, new Float64Array(rows));
      for (let i = 0; i < rows; i++) assert.ok(Object.is(rowSimilarity(flat, d, q, 0.5, i), out[i]));
    }
  }
});
