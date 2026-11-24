// Settings → GPU's Test GPU, and the clean-up after an evaluation that was stopped with Use GPU on (service.ts): each
// in a thread of its own, so the server never waits on the driver.
//
// workerData: { action: 'test' | 'reset' }   out: true | false (it worked)
import { parentPort, workerData } from 'node:worker_threads';
import { resetGpu, testGpu } from './accelerate.ts';

const { action } = workerData as { action: 'test' | 'reset' };
parentPort!.postMessage(action === 'test' ? await testGpu() : await resetGpu());
