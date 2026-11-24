import { fmtPace, fmtWait, paceView, SCOPE_LABEL, shortHost, useRate, useTicker, type PaceView } from '../rate.ts';
import { StatusSlot } from './common.tsx';

/** Current pacing state for one host (default: the active one), re-evaluated while a countdown runs. */
export function usePaceView(host?: string | null) {
  const rate = useRate();
  const view = paceView(rate, host);
  useTicker(view.kind === 'retry' || view.kind === 'pacing' || view.kind === 'idle', view.kind === 'pacing' ? 100 : 250);
  return { rate, view };
}


/**
 * One line that tells apart the states of the Riot API pipeline:
 *   Pacing (next request in X ms) · Rate limited by Riot (Retry-After) · Up to date – no new work.
 */
export function PaceLine({ view }: { view: PaceView }) {
  // 0.18.0: always rendered, always one line. Between two requests the view flips between "pacing" and "sending"
  // several times a second; both now read "Pacing" with a value of fixed width, so nothing on or below the line moves.
  const where = 'host' in view ? ` on ${shortHost(view.host)}` : '';
  const content = (() => {
    switch (view.kind) {
      case 'quiet':
        return null;
      case 'retry':
        return { tone: 'warn', icon: '⏸', label: 'Rate limited by Riot (Retry-After)', detail: `${SCOPE_LABEL[view.scope]}${where}`, value: fmtWait(view.ms) };
      case 'pacing':
        return { tone: 'calm', icon: '⏱', label: 'Pacing', detail: `${fmtPace(view.paceMs)}${where}`, value: `next request in ${fmtWait(view.ms)}` };
      case 'sending':
        return { tone: 'calm', icon: '⏱', label: 'Pacing', detail: `${fmtPace(view.paceMs)}${where}`, value: 'sending' };
      case 'idle':
        return { tone: 'ok', icon: '✓', label: 'Up to date – no new work', detail: '', value: `next check in ${fmtWait(view.ms)}` };
    }
  })();
  if (!content) return <StatusSlot className="mt-1.5 text-[11px]" />;

  const tone = content.tone === 'warn' ? 'text-gold-400' : content.tone === 'ok' ? 'text-[#2dbf8d]' : 'text-ash-400';
  const full = [content.label, content.value, content.detail].filter(Boolean).join(' · ');
  // The part that changes several times a second goes last, so nothing after it can move.
  const short = content.value.replace('next request in', 'next in');
  return (
    <StatusSlot className={`mt-1.5 text-[11px] ${tone}`} title={full}>
      <span aria-hidden="true">{content.icon} </span><b className="font-semibold">{content.label}</b>
      {content.detail && <span className="text-ash-600"> · {content.detail}</span>}
      <span className="tabular-nums"> · {short}</span>
    </StatusSlot>
  );
}
