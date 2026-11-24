import { useEffect, useState } from 'react';
import type { BuildMode } from '../../shared/types.ts';
import { api, img, pct, useRealtime, wrClass, type Meta } from './api.ts';
import { DataPanel, FetchSummary, PatchesCard, ToolsCard, useDbStats } from './components/DataPanel.tsx';
import { DraftAdjustments, ModeToggle, ModelCard } from './components/DraftAdjust.tsx';
import { EvalHistoryCard } from './components/EvalHistory.tsx';
import { BackupCard } from './components/Backup.tsx';
import { FactsCard } from './components/Facts.tsx';
import { ImportButton } from './components/Import.tsx';
import { Items, Summoners } from './components/Items.tsx';
import { LiveGame } from './components/Live.tsx';
import { LivePerformancePanel } from './components/LivePerformance.tsx';
import { Matchup } from './components/Matchup.tsx';
import { YourGames } from './components/Review.tsx';
import { Runes } from './components/Runes.tsx';
import { SettingsModal } from './components/SettingsModal.tsx';
import { SkillPath, SkillPriority } from './components/Skills.tsx';
import { LANE_LABEL, laneNote, takeLegacyLane } from './lane.ts';

const LCU_LABEL = { searching: 'Client not found', connecting: 'Connecting…', connected: 'Client connected', disconnected: 'Client closed' };

/** The League client's gameflow phase in words; nothing for the idle phase. "(simulated)" is kept. */
const PHASE_LABEL: Record<string, string> = {
  Lobby: 'Lobby', Matchmaking: 'In queue', ReadyCheck: 'Match found', ChampSelect: 'Champ select', GameStart: 'Loading',
  InProgress: 'In game', Reconnect: 'Reconnecting', WaitingForStats: 'Post-game', PreEndOfGame: 'Post-game', EndOfGame: 'Post-game',
};
function phaseLabel(phase: string) {
  const [base, ...rest] = phase.split(' ');
  if (!base || base === 'None') return null;
  return [PHASE_LABEL[base] ?? base, ...rest].join(' ');
}

type Tab = 'build' | 'games' | 'data';
const TABS: { id: Tab; label: string }[] = [{ id: 'build', label: 'Build' }, { id: 'games', label: 'Your games' }, { id: 'data', label: 'Data' }];
const readTab = (): Tab => { try { const t = localStorage.getItem('tab'); return t === 'games' || t === 'data' ? t : 'build'; } catch { return 'build'; } };

export function App() {
  const rt = useRealtime();
  const [tab, setTabState] = useState<Tab>(readTab);
  const setTab = (t: Tab) => { setTabState(t); try { localStorage.setItem('tab', t); } catch { /* private mode */ } };
  const [meta, setMeta] = useState<Meta | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // Your Draft-adjusted / Lane baseline click shows at once; the server saves it and pushes the build for every window.
  const [modeClick, setModeClick] = useState<BuildMode | null>(null);

  useEffect(() => { api.meta().then(setMeta).catch((e) => setError(String(e))); }, [rt.patchVersion]);
  // The lane an older version kept in this browser moves to the server once (it is shared with the overlay there).
  useEffect(() => {
    if (rt.matchup?.phase !== 'idle') return;
    const lane = takeLegacyLane();
    if (lane && lane !== rt.matchup.lane) void api.setMatchup({ lane }).catch(() => {});
  }, [rt.matchup?.phase]);   // the lane is read when the phase allows it, not on every change

  const res = rt.build?.response ?? null;
  useEffect(() => { if (modeClick && res?.mode === modeClick) setModeClick(null); }, [res?.mode, modeClick]);

  if (error && !meta) return <div className="boot">Middleware unreachable ({error}). In development, check that <code>npm run dev</code> is running.</div>;
  if (!meta || !rt.matchup || !res) return <div className="boot">Loading Irelia data…</div>;

  const m = rt.matchup;
  const shownMode: BuildMode = res.draft ? modeClick ?? res.mode : 'baseline';
  const rec = shownMode === 'draft' && res.draft ? res.draft.build : res.baseline;
  const chooseMode = (mode: BuildMode) => { setModeClick(mode); api.setMode(mode).catch(() => setModeClick(null)); };
  const updating = rt.build?.key !== m.key;
  const enemy = m.opponentId !== null ? meta.champions.find((c) => c.id === m.opponentId) : undefined;
  const lcuStatus = rt.lcu?.status ?? 'searching';
  const lane = LANE_LABEL[m.lane];
  // Late-game swaps (0.17.0) come with the draft-adjusted build, whichever build is shown; the switch as saved shows at
  // once (the model's status), the build's own info until it arrives.
  const swapsInfo = (rt.model?.info ?? res.model)?.swaps;
  const swaps = swapsInfo?.active && res.draft?.swaps?.length ? { advice: res.draft.swaps, choice: swapsInfo.choice, gate: swapsInfo.gate } : null;

  return (
    <div className="app">
      <header className="top">
        <div className="champ-head">
          <img className="portrait" src={img.champ(meta.version, 'Irelia')} width={72} height={72} alt="Irelia" />
          <div>
            <h1>Irelia Build <span className="for">for</span> {lane} {enemy && <span className="vs">vs {enemy.name}</span>}</h1>
            <p className="sub">
              {shownMode === 'draft' ? 'High-elo Irelia builds adjusted for this draft' : 'Irelia OTP builds from high-elo match data'} · Patch <b>{rec.patch}</b>
              {rec.sample.patches.length > 1 && <span className="warn"> (+{rec.sample.patches[1]} at reduced weight)</span>}
            </p>
          </div>
          {enemy && <img className="portrait enemy" src={img.champ(meta.version, enemy.key)} width={56} height={56} alt={enemy.name} />}
        </div>
        <div className="status">
          <span className={`dot ${rt.connected ? 'ok' : 'bad'}`} /> Middleware
          <span className={`dot ${lcuStatus === 'connected' ? 'ok' : lcuStatus === 'connecting' ? 'warn' : 'bad'}`} /> {LCU_LABEL[lcuStatus]}
          {rt.lcu?.summonerName && <span className="muted">· {rt.lcu.summonerName}</span>}
          {rt.lcu && phaseLabel(rt.lcu.phase) && <span className="pill">{phaseLabel(rt.lcu.phase)}</span>}
          <button type="button" onClick={() => setSettingsOpen(true)} aria-label="Settings" title="Settings: storage, API key, overlay"
            className="ml-2 rounded-md border border-ink-600 px-2.5 py-1 text-sm text-ash-200 hover:border-blood-500 hover:text-ash-50">⚙ Settings</button>
        </div>
      </header>

      <nav className="mb-4 flex gap-1 border-b border-ember" aria-label="Views">
        {TABS.map((t) => (
          <button key={t.id} type="button" onClick={() => setTab(t.id)} aria-current={tab === t.id ? 'page' : undefined}
            className={`-mb-px flex items-center gap-1.5 border-b-2 px-4 py-2 text-sm font-semibold transition-colors ${tab === t.id
              ? 'border-blood-500 text-ash-50'
              : 'border-transparent text-ash-600 hover:text-ash-200'}`}>
            {t.label}
            {t.id === 'build' && rt.live && <span className="size-1.5 rounded-full bg-[#2dbf8d]" title="In game" />}
            {t.id === 'data' && rt.scraper?.running && <span className="size-1.5 rounded-full bg-[#2dbf8d]" title="Fetching" />}
          </button>
        ))}
      </nav>

      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} storageProgress={rt.storage} storageAction={rt.storageAction} scraper={rt.scraper} />

      {tab === 'build' && <>
        {rt.live?.performance && <div className="mb-3"><LivePerformancePanel meta={meta} live={rt.live} /></div>}
        <Matchup meta={meta} lcu={rt.lcu} m={m} />

        <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <ModeToggle mode={shownMode} status={rt.model} available={!!res.draft} onMode={chooseMode} />
          <span className="flex flex-wrap items-center gap-2">
            {updating && <span className="pill live" role="status">Updating build…</span>}
            {m.phase === 'draft' && rt.build && <ImportButton buildKey={rt.build.key} mode={shownMode} realDraft={rt.lcu?.phase === 'ChampSelect'} stale={updating} />}
          </span>
        </div>

        <div className={`stat-strip ${updating ? 'opacity-70' : ''}`}>
          <div><span className={`big ${wrClass(rec.winRate)}`}>{pct(rec.winRate)}</span><label>Win rate</label></div>
          {enemy && <div><span className="big">{rec.sample.matchupGames}</span><label>Matchup games</label></div>}
          <div title={`${rec.sample.totalGames} valid Irelia OTP games across every lane`}>
            <span className="big">{rec.sample.laneGames}</span><label>Valid {lane} games</label>
          </div>
          <div><span className="big">{rec.patch}</span><label>Patch</label></div>
          <div className="note">
            <span>
              {shownMode === 'draft' && res.draft
                ? `Draft-adjusted: every ${lane} game counts by how much its draft looks like this one${enemy ? `, ${enemy.name} games most` : ''}.`
                : rec.sample.usedFallback
                  ? `Only ${rec.sample.matchupGames} ${lane} games vs ${enemy?.name}: blended with other ${lane} matchups.`
                  : enemy ? `Build based on ${lane} ${enemy.name} matchup games only.` : `Build across all ${lane} matchups.`}
            </span>
            {laneNote(rec) && <span className="warn">{laneNote(rec)}</span>}
          </div>
        </div>

        <main className={`grid ${updating ? 'opacity-70 transition-opacity' : ''}`}>
          <div className="col-main">
            {shownMode === 'draft' && <DraftAdjustments meta={meta} res={res} regions={m.regions} status={rt.model} />}
            <div className="row-runes">
              <Runes meta={meta} runes={rec.runes} />
              <div className="stack">
                <Summoners meta={meta} rec={rec} />
                <SkillPriority meta={meta} skills={rec.skills} />
              </div>
            </div>
            <SkillPath meta={meta} skills={rec.skills} />
            <Items meta={meta} rec={rec} swaps={swaps} />
          </div>
          <aside className="col-side">
            {m.phase === 'game' || rt.live
              ? <LiveGame meta={meta} live={rt.live} mode={shownMode} />
              : <FactsCard meta={meta} facts={res.facts} opponentId={m.opponentId} />}
            <FetchSummary scraper={rt.scraper} onOpen={() => setTab('data')} />
          </aside>
        </main>
      </>}

      {tab === 'games' && <YourGames meta={meta} lane={m.lane} refresh={`${rt.model?.info?.dataVersion ?? ''}|${rt.scraper?.me?.games ?? 0}`} />}

      {tab === 'data' && <DataTab rt={rt} validation={res.baseline.validation} />}
    </div>
  );
}

function DataTab({ rt, validation }: { rt: ReturnType<typeof useRealtime>; validation: Parameters<typeof PatchesCard>[0]['validation'] }) {
  const [stats, setStats] = useDbStats(rt.scraper);
  return (
    <div className="grid items-start gap-4 lg:grid-cols-2">
      <div className="space-y-4">
        <DataPanel scraper={rt.scraper} stats={stats} />
        <PatchesCard stats={stats} validation={validation} onChanged={() => void api.stats().then(setStats).catch(() => {})} />
      </div>
      <div className="space-y-4">
        <BackupCard status={rt.backup} />
        <ModelCard status={rt.model} jobs={rt.jobs} />
        <EvalHistoryCard refresh={rt.jobs?.last.evaluate?.finishedAt ?? null} />
        <ToolsCard backfill={rt.scraper?.backfill ?? null} jobs={rt.jobs} />
      </div>
    </div>
  );
}
