// Transfer thread (server/transfer.ts): one pool export or import, then it exits. Copying a
// database and compressing it takes seconds to minutes; here the dashboard, fetching and the live game carry on.
//
// workerData: { dataDir, versions, app, job }   job: see TransferJob
// out:        { progress: { label, fraction } } while it runs, then { ok: true, result } | { ok: false, error }
import { parentPort, workerData } from 'node:worker_threads';
import { setDataDir } from './config.ts';
import { exportPool, importExport } from './transfer.ts';
import type { TransferJob } from './transfers.ts';

const boot = workerData as { dataDir: string; versions: string[]; app: string; job: TransferJob };
setDataDir(boot.dataDir);
const progress = (p: { label: string; fraction: number | null }) => parentPort!.postMessage({ progress: p });

async function run(job: TransferJob) {
  switch (job.kind) {
    case 'export-pool': return exportPool(boot.dataDir, job.folder, { app: boot.app, progress });
    case 'import': return importExport(boot.dataDir, job.file, { confirmed: true, progress });
  }
}

try {
  parentPort!.postMessage({ ok: true, result: await run(boot.job) });
} catch (err) {
  parentPort!.postMessage({ ok: false, error: err instanceof Error ? err.message : String(err) });
}
