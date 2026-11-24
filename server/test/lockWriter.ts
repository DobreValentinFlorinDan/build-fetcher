// Test helper (server/dbLocks.test.ts): writes like the fetching thread, in a thread of its own, until told to stop.
// Each write is a transaction that reads before it writes (Sourcing.queueHistory's shape). Posts how many went in,
// the slowest, every error, and when each one went in.
import { parentPort, workerData } from 'node:worker_threads';
import { openDb } from '../db.ts';

const { file, busyTimeoutMs } = workerData as { file: string; busyTimeoutMs?: number };
const db = openDb(file, { migrate: false, busyTimeoutMs });
let n = 0, writes = 0, slowest = 0;
const errors: string[] = [], at: number[] = [];
const timer = setInterval(() => {
  const id = `LOCK_${n++}`;
  const t = performance.now();
  try {
    db.tx(() => {
      if (!db.hasMatch(id)) db.enqueue({ match_id: id, platform: 'KR', kind: 'match', puuid: 'lock', source: 'otp', score: 1 });
      db.setScanned('lock');
    });
    db.dequeue(id);
    writes++;
    at.push(Date.now());
  } catch (err) { errors.push(`${(err as Error).message} after ${Math.round(performance.now() - t)} ms`); }
  slowest = Math.max(slowest, performance.now() - t);
}, 20);
parentPort!.on('message', () => {
  clearInterval(timer);
  parentPort!.postMessage({ writes, slowest: Math.round(slowest), errors, at });
  db.close();
  parentPort!.close();
});
parentPort!.postMessage('ready');
