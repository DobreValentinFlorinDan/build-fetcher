/**
 * Runs `fetch` for up to `concurrency` items at a time, but hands the results to `commit` strictly in input
 * order. Database writes, log lines, counters and early stops therefore come out exactly as a sequential loop
 * would produce them; only the waiting (rate-limiter slot, network) overlaps. The rate limiter spaces the
 * request starts, so a larger pool never sends faster: it just keeps the next request ready when its slot opens.
 *
 * `commit` returns false to stop early. Items not started yet are skipped, and the outstanding ones are aborted:
 * one still waiting for a slot never goes out. Errors surface in order: an item's failure is thrown when its turn
 * to commit comes, and failures of items after a stop or an earlier error are ignored.
 */
export async function inOrder<T, R>(
  items: readonly T[],
  { concurrency, signal }: { concurrency: number; signal: AbortSignal },
  fetch: (item: T, index: number, signal: AbortSignal) => Promise<R>,
  commit: (result: R, item: T, index: number) => boolean | void,
): Promise<void> {
  const stop = new AbortController();
  const linked = AbortSignal.any([signal, stop.signal]);
  const started = new Map<number, Promise<R>>();
  const width = Math.max(1, Math.floor(concurrency));
  let launched = 0;

  const fill = (cursor: number) => {
    while (launched < items.length && launched - cursor < width) {
      const i = launched++;
      const p = (async () => fetch(items[i], i, linked))();
      p.catch(() => {});   // surfaced when its turn comes; nobody waits for it after a stop
      started.set(i, p);
    }
  };

  try {
    for (let i = 0; i < items.length; i++) {
      signal.throwIfAborted();
      fill(i);
      const result = await started.get(i)!;
      started.delete(i);
      signal.throwIfAborted();
      if (commit(result, items[i], i) === false) break;
    }
  } finally {
    if (started.size) {
      stop.abort(new Error('stopped'));
      await Promise.allSettled(started.values());
    }
  }
}
