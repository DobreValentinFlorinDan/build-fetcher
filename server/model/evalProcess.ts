// One of an evaluation's member processes (0.22.0, model/evalTeam.ts leadProcessTeam), started by the job thread
// (model/runEval.ts startTeam) in the desktop app: its own process, so its own 4 GB JavaScript heap, instead of a thread
// sharing the app's. Its first message names the file holding the job thread's snapshot (and the purchases' chances);
// it reads its copy, says ready, and runs the same comparisons, taking the chunks rank 0 hands it. Its report goes
// nowhere. It ends when the evaluation is over, or when the app goes (its message channel closes).
import fs from 'node:fs';
import v8 from 'node:v8';
import { collectGarbage } from '../gc.ts';
import { contextOf, evaluateWith, type EvalSnapshot } from './evaluate.ts';
import { joinProcessTeam, type LeaderMessage, type MemberMessage } from './evalTeam.ts';

/** The first message from the job thread. */
export interface ProcessBoot { type: 'boot'; snapshot: string; itemP: string | null; rank: number; size: number }

const send = (msg: MemberMessage) => { process.send?.(msg); };
process.on('disconnect', () => process.exit(0));

const boot = await new Promise<ProcessBoot>((resolve) => {
  const first = (msg: unknown) => {
    if ((msg as ProcessBoot)?.type !== 'boot') return;
    process.off('message', first);
    resolve(msg as ProcessBoot);
  };
  process.on('message', first);
});
const snapshot = v8.deserialize(fs.readFileSync(boot.snapshot)) as EvalSnapshot;
if (boot.itemP) {
  const b = fs.readFileSync(boot.itemP);
  snapshot.itemP = new Float64Array(b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength));
}
const team = joinProcessTeam(boot.rank, boot.size, send, (fn) => process.on('message', (msg) => fn(msg as LeaderMessage)));
send({ type: 'ready' });
await evaluateWith(contextOf(snapshot, () => collectGarbage(), collectGarbage), team);
await team.close();
process.disconnect?.();
