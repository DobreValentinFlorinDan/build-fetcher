// One of an evaluation's other threads (0.11.0, model/evalTeam.ts), started by the job thread (model/runEval.ts). It
// gets a copy of what the job thread read (`blob`: the snapshot serialised once, shared by every thread), runs the
// same comparisons and takes its share of each loop's games. Its own report goes nowhere: the job thread's is kept.
//
// workerData: { entry?, blob: SharedArrayBuffer, itemP?: SharedArrayBuffer (the purchases' chances, shared as they are), rank,
//              size, port: MessagePort, memory: TeamMemory, gpu?: SharedArrayBuffer (Use GPU: the job thread's session
//              stats, server/gpu/accelerate.ts), control?: SharedArrayBuffer | null (the busy flag) }
import v8 from 'node:v8';
import { workerData, type MessagePort } from 'node:worker_threads';
import { collectGarbage } from '../gc.ts';
import { startGpu } from '../gpu/accelerate.ts';
import { contextOf, evaluateWith, type EvalSnapshot } from './evaluate.ts';
import { joinTeam, type TeamMemory } from './evalTeam.ts';

const boot = workerData as {
  blob: SharedArrayBuffer; itemP?: SharedArrayBuffer; rank: number; size: number; port: MessagePort; memory: TeamMemory;
  gpu?: SharedArrayBuffer; control?: SharedArrayBuffer | null;
};
const snapshot = v8.deserialize(new Uint8Array(boot.blob)) as EvalSnapshot;
if (boot.itemP) snapshot.itemP = new Float64Array(boot.itemP);
const team = joinTeam(boot.rank, boot.size, boot.port, boot.memory);
// Use GPU: the job thread checked it; this thread counts into its stats, and the job thread frees what it put there.
const flags = boot.control ? new Int32Array(boot.control) : null;
if (boot.gpu) await startGpu({ thread: `thread ${boot.rank + 1}`, rank: boot.rank, stats: boot.gpu, busy: flags ? () => Atomics.load(flags, 0) === 1 : undefined });
// At each step and before each fold's models, what the last left behind goes (as in the job thread).
await evaluateWith(contextOf(snapshot, () => collectGarbage(), collectGarbage), team);
await team.close();
