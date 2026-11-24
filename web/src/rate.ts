import { useEffect, useState, useSyncExternalStore } from 'react';
import type { ReceivedRate } from '../../shared/rateView.ts';
import type { RateSnapshot } from '../../shared/types.ts';

export { fmtPace, fmtWait, paceView, pickHost, SCOPE_LABEL, shortHost, type PaceView } from '../../shared/rateView.ts';

export type LiveRate = ReceivedRate;

// The latest pacing snapshot lives outside React state: it updates several times a second while requests
// run (/ws `ratelimit`), and only the components that show it (the Match Database panel) should re-render.
let current: LiveRate | null = null;
const listeners = new Set<() => void>();

export const rateStore = {
  /** Keeps the newest snapshot (by worker time), whichever channel delivered it. */
  offer(snapshot: RateSnapshot | null | undefined) {
    if (!snapshot || (current && snapshot.at < current.at)) return;
    current = { ...snapshot, receivedAt: Date.now() };
    for (const l of listeners) l();
  },
  subscribe(listener: () => void) {
    listeners.add(listener);
    return () => { listeners.delete(listener); };
  },
  get: () => current,
};

export const useRate = () => useSyncExternalStore(rateStore.subscribe, rateStore.get);

/** Re-renders every `ms` while `active` (countdowns). */
export function useTicker(active: boolean, ms = 100) {
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!active) return;
    const t = setInterval(() => setTick((n) => n + 1), ms);
    return () => clearInterval(t);
  }, [active, ms]);
}
