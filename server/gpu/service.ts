import { startWorker } from '../workers.ts';
import { gpuLog } from './log.ts';

let testing: Promise<boolean> | null = null;

/** Test GPU (Settings → GPU), in its own thread (worker.ts); what it finds goes in the log. One at a time. */
export function testGpuNow(): Promise<boolean> {
  return (testing ??= run('test').finally(() => { testing = null; }));
}
export const gpuTesting = () => testing !== null;

/**
 * After an evaluation with Use GPU stopped before its end (Cancel, champ select, an error): its threads couldn't free
 * what they put on the card, so it is freed here.
 */
export const freeStoppedEvaluation = () => run('reset');

function run(action: 'test' | 'reset'): Promise<boolean> {
  return new Promise((resolve) => {
    let worker: ReturnType<typeof startWorker>;
    try { worker = startWorker('gpu', { action }); } catch (err) {
      gpuLog(`${action === 'test' ? 'Test GPU' : 'Freeing the GPU'}: ${err instanceof Error ? err.message : String(err)}`);
      resolve(false);
      return;
    }
    worker.once('message', (ok: unknown) => resolve(ok === true));
    worker.once('error', (err: Error) => { gpuLog(`${action === 'test' ? 'Test GPU' : 'Freeing the GPU'}: ${err.message}`); resolve(false); });
    worker.once('exit', () => resolve(false));
  });
}
