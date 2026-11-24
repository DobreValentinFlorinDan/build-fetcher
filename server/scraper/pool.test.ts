import assert from 'node:assert/strict';
import { test } from 'node:test';
import { inOrder } from './pool.ts';

const later = <T>(ms: number, value: T, signal?: AbortSignal) =>
  new Promise<T>((resolve, reject) => {
    const t = setTimeout(() => resolve(value), ms);
    signal?.addEventListener('abort', () => { clearTimeout(t); reject(signal.reason); }, { once: true });
  });

test('results are committed in input order even when they arrive out of order', async () => {
  const delays = [30, 5, 20, 1, 10, 2];
  const committed: number[] = [];
  let running = 0, peak = 0;
  await inOrder(delays, { concurrency: 3, signal: new AbortController().signal },
    async (ms, i) => { peak = Math.max(peak, ++running); try { return await later(ms, i); } finally { running--; } },
    (value, _ms, i) => { assert.equal(value, i); committed.push(i); });
  assert.deepEqual(committed, [0, 1, 2, 3, 4, 5]);
  assert.equal(peak, 3);
});

test('a stop skips the items not started yet and aborts the outstanding ones', async () => {
  const started: number[] = [];
  const aborted: number[] = [];
  const committed: number[] = [];
  await inOrder([0, 1, 2, 3, 4, 5, 6, 7], { concurrency: 3, signal: new AbortController().signal },
    async (i, _idx, signal) => {
      started.push(i);
      try { return await later(i === 2 ? 1 : 50, i, signal); } catch (err) { aborted.push(i); throw err; }
    },
    (i) => { committed.push(i); return i < 2; });   // stop after committing item 2
  assert.deepEqual(committed, [0, 1, 2]);
  assert.ok(!started.includes(7), 'items past the window never start');
  assert.ok(aborted.length > 0 && aborted.every((i) => i > 2), `outstanding ones aborted: ${aborted}`);
});

test('errors surface in order and cancellation propagates', async () => {
  const committed: number[] = [];
  await assert.rejects(
    inOrder([0, 1, 2, 3], { concurrency: 3, signal: new AbortController().signal },
      async (i) => { if (i === 1) throw new Error('item 1 failed'); if (i === 2) throw new Error('item 2 failed'); return later(5, i); },
      (i) => { committed.push(i); }),
    /item 1 failed/);
  assert.deepEqual(committed, [0]);

  const cancel = new AbortController();
  setTimeout(() => cancel.abort(new Error('scan cancelled')), 20);
  await assert.rejects(
    inOrder([0, 1, 2, 3, 4], { concurrency: 2, signal: cancel.signal }, (i, _idx, signal) => later(15, i, signal), () => {}),
    /scan cancelled/);
});
