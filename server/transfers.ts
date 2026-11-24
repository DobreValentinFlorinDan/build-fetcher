// Runs transfers (server/transfer.ts: pool exports and imports) one at a time in the transfer thread, with their
// progress for the dashboard. A second one asked for while one runs waits its turn (auto-export) or is refused (a click:
// the button says what is running).
import { EventEmitter } from 'node:events';
import path from 'node:path';
import type { TransferProgress } from '../shared/types.ts';
import { startWorker } from './workers.ts';

export type TransferJob =
  | { kind: 'export-pool'; folder: string }
  | { kind: 'import'; file: string };

const LABEL: Record<TransferJob['kind'], string> = { 'export-pool': 'Backing up the pool', import: 'Importing' };
const KIND: Record<TransferJob['kind'], TransferProgress['kind']> = { 'export-pool': 'export', import: 'import' };

export function describe(job: TransferJob): string {
  return job.kind === 'import' ? `${LABEL.import} ${path.basename(job.file)}` : LABEL[job.kind];
}

export class Transfers extends EventEmitter {
  status: TransferProgress | null = null;
  private running: Promise<unknown> | null = null;
  private waiting: (() => void)[] = [];

  constructor(private boot: () => { dataDir: string; versions: string[]; app: string }) { super(); }

  get busy() { return this.running !== null; }

  /** Runs `job`; with `wait`, after the one running now, else refused while one runs. Resolves with its result. */
  async run<T = unknown>(job: TransferJob, o: { wait?: boolean; result?: (r: T) => string } = {}): Promise<T> {
    if (this.running) {
      if (!o.wait) throw new Error(`${this.status?.label ?? 'A transfer'} is still running: wait for it to finish.`);
      await new Promise<void>((resolve) => this.waiting.push(resolve));
    }
    const label = describe(job);
    this.status = { kind: KIND[job.kind], label, fraction: 0, startedAt: Date.now(), finishedAt: null, error: null, result: null };
    this.emit('status', this.status);
    const promise = new Promise<T>((resolve, reject) => {
      const worker = startWorker('transfer', { ...this.boot(), job });
      let last = 0;
      worker.on('message', (msg: { progress?: { label: string; fraction: number | null }; ok?: boolean; result?: T; error?: string }) => {
        if (msg.progress) {
          this.status = { ...this.status!, label: `${label} · ${msg.progress.label}`, fraction: msg.progress.fraction };
          if (Date.now() - last > 300) { last = Date.now(); this.emit('status', this.status); }
        } else if (msg.ok) resolve(msg.result as T);
        else if (msg.ok === false) reject(new Error(msg.error));
      });
      worker.on('error', reject);
      worker.on('exit', (code) => { if (code !== 0) reject(new Error(`The transfer stopped (exit ${code}).`)); });
    });
    this.running = promise;
    try {
      const r = await promise;
      const file = (r as { file?: string } | null)?.file ?? null;
      this.status = { ...this.status!, label, fraction: 1, finishedAt: Date.now(), result: o.result ? o.result(r) : 'Done', file };
      return r;
    } catch (err) {
      this.status = { ...this.status!, label, finishedAt: Date.now(), error: err instanceof Error ? err.message : String(err) };
      throw err;
    } finally {
      this.running = null;
      this.emit('status', this.status);
      this.waiting.shift()?.();
    }
  }
}
