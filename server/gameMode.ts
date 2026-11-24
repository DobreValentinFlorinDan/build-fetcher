// Champ select and games (0.14.0): the middleware gives way to League. While one is on, the middleware process runs
// at below-normal priority, so the game's threads win every contest for a core; when champ select starts, every
// thread collects its garbage and, on Windows, the process hands the pages it no longer touches back to the system.
import os from 'node:os';
import v8 from 'node:v8';
import vm from 'node:vm';

/** A working-set trim that frees less than this is not worth its page faults afterwards: it isn't done again. */
export const TRIM_MIN_BYTES = 50 * 1024 * 1024;

type Kernel32 = { trim: () => boolean };
let kernel32: Promise<Kernel32 | null> | null = null;
/** SetProcessWorkingSetSize(GetCurrentProcess(), -1, -1) through koffi, on Windows only; null elsewhere or if it fails. */
function loadKernel32(): Promise<Kernel32 | null> {
  if (process.platform !== 'win32') return Promise.resolve(null);
  // The bundled app carries koffi next to its bundles (scripts/bundle.mjs); a source run takes it from node_modules.
  const spec = import.meta.url.endsWith('.ts') ? 'koffi' : new URL('./native/koffi/index.js', import.meta.url).href;
  return (kernel32 ??= import(spec).then((m: { default?: unknown }) => {
    const koffi = (m.default ?? m) as { load: (p: string) => { func: (sig: string) => (...a: unknown[]) => unknown } };
    const lib = koffi.load('kernel32.dll');
    const current = lib.func('void * __stdcall GetCurrentProcess()');
    const setSize = lib.func('bool __stdcall SetProcessWorkingSetSize(void *hProcess, intptr_t dwMinimumWorkingSetSize, intptr_t dwMaximumWorkingSetSize)');
    return { trim: () => setSize(current(), -1, -1) === true };
  }).catch(() => null));
}

/** The main thread's own full collection (the threads have gc exposed; the main thread gets it this way). */
let mainGc: (() => void) | null | undefined;
function collectMain() {
  if (mainGc === undefined) {
    try { v8.setFlagsFromString('--expose-gc'); mainGc = vm.runInNewContext('gc') as () => void; } catch { mainGc = null; }
  }
  mainGc?.();
}

export interface GameModeOptions {
  /** Each worker thread's full garbage collection (the scraper's, the model's), resolved when done. */
  collect: () => Promise<void>[];
  log?: (line: string) => void;
  /** Tests: the process's priority, and its memory. */
  setPriority?: (priority: number) => void;
  rss?: () => number;
  trim?: () => Promise<boolean>;
}

export class GameMode {
  private on = false;
  /** Whether trimming the working set still pays (false once one freed less than TRIM_MIN_BYTES). */
  private trimPays = true;
  private cleaning: Promise<void> | null = null;
  /** What the last clean-up did, for the log and the tests. */
  last: { before: number; collected: number; after: number; trimmed: boolean } | null = null;

  constructor(private o: GameModeOptions) {}

  /** Champ select or a game began (true) or ended (false). */
  set(busy: boolean) {
    if (busy === this.on) return;
    this.on = busy;
    this.priority(busy ? os.constants.priority.PRIORITY_BELOW_NORMAL : os.constants.priority.PRIORITY_NORMAL);
    if (busy) this.cleaning ??= this.cleanUp().finally(() => { this.cleaning = null; });
  }

  /** The clean-up of the last set(true), if it is still running. */
  get pending() { return this.cleaning; }

  private priority(p: number) {
    try { (this.o.setPriority ?? ((x: number) => os.setPriority(x)))(p); } catch (err) {
      this.o.log?.(`[game] could not set the middleware's priority: ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  private async cleanUp() {
    const rss = this.o.rss ?? (() => process.memoryUsage().rss);
    const before = rss();
    await Promise.all(this.o.collect());
    collectMain();
    const collected = rss();
    let trimmed = false;
    if (this.trimPays && this.on) {
      try { trimmed = await (this.o.trim ?? (async () => (await loadKernel32())?.trim() ?? false))(); } catch { trimmed = false; }
    }
    const after = rss();
    this.last = { before, collected, after, trimmed };
    const mb = (b: number) => `${Math.round(b / 1048576)} MB`;
    const line = `[game] champ select: ${mb(before)}, ${mb(collected)} after collecting garbage`;
    if (!trimmed) { this.o.log?.(`${line}.`); return; }
    if (collected - after < TRIM_MIN_BYTES) {
      // Not worth it here: the pages come back as soon as they are touched. The next champ selects only collect.
      this.trimPays = false;
      this.o.log?.(`${line}, ${mb(after)} after the working-set trim: under ${mb(TRIM_MIN_BYTES)} freed, so it isn't done again.`);
    } else this.o.log?.(`${line}, ${mb(after)} after the working-set trim.`);
  }
}
