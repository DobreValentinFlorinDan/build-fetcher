// Irelia builds are collected and recommended per lane. Every source names positions the same way once
// upper-cased: match-v5 `teamPosition`, the LCU's champ-select `assignedPosition` and the Live Client's `position`.
import type { Lane } from './types.ts';

export const LANES: readonly Lane[] = ['TOP', 'MIDDLE'];

export const LANE_LABEL: Record<Lane, string> = { TOP: 'Top', MIDDLE: 'Mid' };

/** "TOP", "middle", "MID" → the lane; jungle, bot, support, "" and UNKNOWN → null (no Irelia lane data there). */
export function toLane(position: string | null | undefined): Lane | null {
  const p = (position ?? '').trim().toUpperCase();
  return p === 'TOP' ? 'TOP' : p === 'MIDDLE' || p === 'MID' ? 'MIDDLE' : null;
}
