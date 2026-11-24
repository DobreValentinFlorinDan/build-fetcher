// A helper thread of Slim stored data's slimming (0.14.4, started by server/slim.ts apply): it slims the games it is
// sent (server/slimPart.ts) and sends back what to write; the slim thread alone writes the cache.
//   in  workerData: { dicts: [id, bytes][], current: id | null }; messages: { id, games: PartGame[] }
//   out { id, results: PartResult[] } | { id, error }
import { parentPort, workerData } from 'node:worker_threads';
import { slimGame, type PartCodec, type PartGame } from './slimPart.ts';

const boot = workerData as { dicts: [number, Uint8Array][]; current: number | null };
const dicts = new Map(boot.dicts.map(([id, bytes]) => [id, Buffer.from(bytes)]));
const codec: PartCodec = {
  get: (id) => dicts.get(id),
  current: boot.current !== null && dicts.has(boot.current) ? { id: boot.current, bytes: dicts.get(boot.current)! } : null,
};

parentPort!.on('message', (msg: { id: number; games: PartGame[] }) => {
  try {
    const results = msg.games.map((g) => slimGame(g, codec));
    parentPort!.postMessage({ id: msg.id, results });
  } catch (err) {
    parentPort!.postMessage({ id: msg.id, error: err instanceof Error ? err.message : String(err) });
  }
});
