import { useEffect, useState } from 'react';
import type { BuildMode } from '../../../shared/types.ts';
import { api } from '../api.ts';

type Result = Awaited<ReturnType<typeof api.importBuild>>;

/**
 * Stage C1: one click saves the build shown as the "Irelia Companion" rune page (made current) and item set (it shows
 * in the in-game shop). Only during a real champ select; the app only ever replaces the page and set it made itself.
 */
export function ImportButton({ buildKey, mode, realDraft, stale }: { buildKey: string; mode: BuildMode; realDraft: boolean; stale: boolean }) {
  const [state, setState] = useState<{ busy: boolean; result?: Result; error?: string }>({ busy: false });
  // A new build (another pick, the other mode) has not been saved yet.
  useEffect(() => { setState({ busy: false }); }, [buildKey, mode]);
  const run = () => {
    setState({ busy: true });
    api.importBuild(buildKey, mode).then((result) => setState({ busy: false, result }),
      (e) => setState({ busy: false, error: e instanceof Error ? e.message : String(e) }));
  };
  const r = state.result;
  const saved = r && [r.runePage.ok && 'rune page', r.itemSet.ok && 'item set'].filter(Boolean).join(' and ');
  return (
    <span className="flex flex-wrap items-center gap-2">
      <button type="button" className="ghost !px-2.5 !py-1 text-[12px]" onClick={run} disabled={state.busy || !realDraft || stale}
        title={stale ? 'The build is being updated for the new pick' : realDraft
          ? `Saves this ${mode === 'draft' ? 'draft-adjusted' : 'lane'} build as the "Irelia Companion" rune page and item set in the League client. Your own pages and sets are never changed.`
          : 'Available during a real champ select'}>
        {state.busy ? 'Saving to the client…' : '⤓ Save runes and items to the client'}
      </button>
      {saved && <span className="text-[11px] text-[#2dbf8d]">Saved the {saved}.</span>}
      {r && !r.runePage.ok && <span className="text-[11px] text-gold-400">Rune page: {r.runePage.error}</span>}
      {r && !r.itemSet.ok && <span className="text-[11px] text-gold-400">Item set: {r.itemSet.error}</span>}
      {state.error && <span className="text-[11px] text-blood-400">{state.error}</span>}
    </span>
  );
}
