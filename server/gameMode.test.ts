// Champ select and games (server/gameMode.ts, 0.14.0): the priority goes down and back, every thread collects its
// garbage first, and the working-set trim is kept only while it frees enough to be worth it.
import assert from 'node:assert/strict';
import os from 'node:os';
import { test } from 'node:test';
import { GameMode, TRIM_MIN_BYTES } from './gameMode.ts';

const MB = 1024 * 1024;

test('gameMode: below-normal priority during champ select and the game, normal after; the threads collect before the trim', async () => {
  const priorities: number[] = [], order: string[] = [];
  let rss = 900 * MB;
  const mode = new GameMode({
    setPriority: (p) => priorities.push(p),
    collect: () => [Promise.resolve().then(() => { order.push('scraper'); rss -= 100 * MB; }), Promise.resolve().then(() => { order.push('model'); })],
    rss: () => rss,
    trim: async () => { order.push('trim'); rss -= 200 * MB; return true; },
  });
  mode.set(true);
  mode.set(true);   // no change: nothing again
  await mode.pending;
  assert.deepEqual(priorities, [os.constants.priority.PRIORITY_BELOW_NORMAL]);
  assert.deepEqual(order, ['scraper', 'model', 'trim']);
  assert.deepEqual(mode.last, { before: 900 * MB, collected: 800 * MB, after: 600 * MB, trimmed: true });
  mode.set(false);
  assert.deepEqual(priorities, [os.constants.priority.PRIORITY_BELOW_NORMAL, os.constants.priority.PRIORITY_NORMAL]);
  // It freed enough: the next champ select trims again.
  mode.set(true);
  await mode.pending;
  assert.equal(order.filter((x) => x === 'trim').length, 2);
});

test('gameMode: a trim that frees under 50 MB is not done again; a failing priority call is logged and ignored', async () => {
  const lines: string[] = [];
  let trims = 0;
  const mode = new GameMode({
    setPriority: () => { throw new Error('denied'); },
    collect: () => [],
    rss: () => 500 * MB - trims * (TRIM_MIN_BYTES - MB),
    trim: async () => { trims++; return true; },
    log: (l) => lines.push(l),
  });
  mode.set(true); await mode.pending; mode.set(false);
  mode.set(true); await mode.pending; mode.set(false);
  assert.equal(trims, 1, 'not again after it freed too little');
  assert.ok(lines.some((l) => /could not set the middleware's priority: denied/.test(l)));
  assert.ok(lines.some((l) => /isn't done again/.test(l)));
});
