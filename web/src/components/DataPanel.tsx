import { useEffect, useRef, useState } from 'react';
import type { BackfillStatus, HostWork, JobsStatus, ScraperStatus, ValidationReport } from '../../../shared/types.ts';
import { api, fmtBytes, useEstimates, useOldPatches, type DbStats } from '../api.ts';
import { desktop } from '../desktop.ts';
import { fmtPace, pickHost, shortHost, useRate } from '../rate.ts';
import { Card, DetailsToggle, StatusSlot, SwitchRow, useShowDetails } from './common.tsx';
import { PaceLine, usePaceView } from './PaceStatus.tsx';
import { fmtAbout, ProgressBar, Segments, Takes } from './Progress.tsx';

// The Data tab (fetching, stored games, patches, maintenance) and the compact fetching card on the Build tab.

const SOURCE_LABEL = { otp: 'OTP', main: 'main', harvested: 'harvested', me: 'yours' } as const;
const num = (n: number | undefined | null) => (n ?? 0).toLocaleString('en-US');

/**
 * Start / Stop fetching. The button answers at once ("Starting…", "Stopping…") and settles when the worker reports
 * back; a click is never swallowed by an earlier one that is still on its way.
 */
function useFetchToggle(scraper: ScraperStatus | null) {
  // Paused during a game (0.14.0) still counts as on: Stop stops it, and it resumes by itself after the game.
  const running = scraper?.running === true || scraper?.paused === 'game';
  const [pending, setPending] = useState<{ want: boolean; at: number } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => { if (pending && running === pending.want) setPending(null); }, [running, pending]);
  useEffect(() => () => clearTimeout(timer.current), []);
  const shown = pending ? pending.want : running;
  const toggle = () => {
    const want = !shown;
    setPending({ want, at: Date.now() });
    clearTimeout(timer.current);
    // No answer within a few seconds: show the reported state again (the error line says why, e.g. no API key).
    timer.current = setTimeout(() => setPending(null), 6000);
    void (want ? api.startScraper() : api.stopScraper()).catch(() => setPending(null));
  };
  return { running: shown, pending: !!pending, toggle };
}

function FetchButton({ scraper }: { scraper: ScraperStatus | null }) {
  const { running, pending, toggle } = useFetchToggle(scraper);
  return (
    <button type="button" className={`fetch-toggle ${running ? 'stop' : 'start'}`} onClick={toggle} aria-pressed={running}>
      {pending ? (running ? 'Starting…' : 'Stopping…') : running ? '■ Stop fetching' : '▶ Start fetching'}
    </button>
  );
}

const fetchPill = (scraper: ScraperStatus | null) => <span className={`pill ${scraper?.running ? 'live' : ''}`}>{scraper?.running ? '● Fetching' : scraper?.paused === 'game' ? '❚❚ Paused for the game' : 'Idle'}</span>;

/** Stored counts and patches, read again when new games arrive (at most every 10 s). */
export function useDbStats(scraper: ScraperStatus | null) {
  const [stats, setStats] = useState<DbStats | null>(null);
  const last = useRef(0);
  const games = scraper?.ireliaGames;
  useEffect(() => {
    const load = () => { last.current = Date.now(); void api.stats().then(setStats).catch(() => {}); };
    const wait = Math.max(0, 10_000 - (Date.now() - last.current));
    const t = setTimeout(load, wait);
    return () => clearTimeout(t);
  }, [games]);
  return [stats, setStats] as const;
}

/**
 * The line under the fetch button. While the background lanes run it sums up the Riot hosts ("Checking players on 5
 * servers · fetching matches in 3 regions") rather than chaining every host's task; otherwise (the backfill, a pause)
 * it is the worker's own phase. Nothing while idle.
 */
function statusLine(scraper: ScraperStatus | null): string | null {
  if (!scraper) return 'Connecting…';
  const hosts = scraper.hosts ?? [];
  if (scraper.running && hosts.length) {
    const platform = hosts.filter((h) => h.kind === 'platform'), regional = hosts.filter((h) => h.kind === 'regional');
    const busyP = platform.filter((h) => h.busy).length, busyR = regional.filter((h) => h.busy).length;
    if (!busyP && !busyR) return null;   // the pacing line says "Up to date"
    const parts = [
      busyP ? `checking players on ${busyP} of ${platform.length} server${platform.length === 1 ? '' : 's'}` : '',
      busyR ? `fetching matches in ${busyR} of ${regional.length} region${regional.length === 1 ? '' : 's'}` : '',
    ].filter(Boolean).join(' · ');
    return parts.charAt(0).toUpperCase() + parts.slice(1);
  }
  return scraper.phase && scraper.phase !== 'idle' ? scraper.phase : null;
}

/** Build tab: fetching at a glance, with the switch. The Data tab has the rest. */
export function FetchSummary({ scraper, onOpen }: { scraper: ScraperStatus | null; onOpen: () => void }) {
  const { view } = usePaceView();
  const line = view.kind !== 'idle' ? statusLine(scraper) : null;
  return (
    <Card title="Match data" aside={fetchPill(scraper)}>
      <FetchButton scraper={scraper} />
      <StatusSlot className="phase">{line}</StatusSlot>
      {scraper?.lastError && <div className="error">{scraper.lastError}</div>}
      <PaceLine view={view} />
      <div className="mt-2 flex items-baseline justify-between gap-2 text-[12px] text-ash-400">
        <span><b className="text-ash-50">{num(scraper?.ireliaGames)}</b> Irelia games · <b className="text-ash-50">{num(scraper?.currentPatchGames)}</b> this patch</span>
        <button type="button" className="ghost !px-2 !py-0.5 text-[11px]" onClick={onOpen}>Details</button>
      </div>
    </Card>
  );
}

/** Data tab: fetching, what is stored, and how the Riot API is paced. */
export function DataPanel({ scraper, stats }: { scraper: ScraperStatus | null; stats: DbStats | null }) {
  const { rate, view } = usePaceView();
  // Bars: the busiest host from the live snapshot (several updates a second), else the 2 s status report.
  const host = rate?.busiest ? pickHost(rate, rate.busiest) : null;
  const windows = host?.windows ?? scraper?.rateWindow ?? [];
  const paceMs = host?.paceMs ?? rate?.defaultPaceMs;
  const sources = scraper?.sources ?? stats?.sources ?? {};
  const line = view.kind !== 'idle' ? statusLine(scraper) : null;
  const [details] = useShowDetails();

  return (
    <Card title="Match Database" aside={fetchPill(scraper)}>
      <FetchButton scraper={scraper} />
      {/* While the loop is up to date the pacing line below says so, with the time to the next check. Each host's own task
          is listed under Riot hosts; this line sums them up. */}
      <StatusSlot className="phase">{line}</StatusSlot>
      {scraper?.lastError && <div className="error">{scraper.lastError}</div>}

      <dl className="stat-grid">
        <div><dt>Apex players</dt><dd>{num(scraper?.playersSeen ?? stats?.players)}</dd></div>
        <div><dt>Irelia OTPs</dt><dd>{num(scraper?.otpPlayers ?? stats?.otp)}</dd></div>
        <div><dt>Irelia mains</dt><dd>{num(scraper?.mainPlayers ?? stats?.mains)}</dd></div>
        <div><dt>Matches scanned</dt><dd>{num(scraper?.matchesScanned ?? stats?.matches)}</dd></div>
        <div><dt>Irelia games</dt><dd>{num(scraper?.ireliaGames ?? stats?.games)}</dd></div>
        {/* The fetcher counts its first platform's own patch; Data Dragon's (stats) can be one ahead on patch day. */}
        <div><dt>On {scraper ? scraper.currentPatch ?? 'this patch' : stats?.currentPatch ?? 'patch'}</dt><dd>{num(scraper?.currentPatchGames ?? stats?.current)}</dd></div>
        <div><dt>API requests</dt><dd>{num(scraper?.requests)}</dd></div>
        <div title="Stored Irelia games by where they came from. Only the lane builds' sources feed them; your own games never do.">
          <dt>By source{stats?.buildSources && <span className="muted"> · lane builds use {stats.buildSources.map((k) => SOURCE_LABEL[k as keyof typeof SOURCE_LABEL] ?? k).join(', ')}</span>}</dt>
          <dd className="sources">
            {(['otp', 'main', 'harvested', 'me'] as const).map((k) => <span key={k}>{SOURCE_LABEL[k]} {num(sources[k])}</span>)}
          </dd>
        </div>
      </dl>

      {/* Always present (0.18.0): it came and went with the pacing snapshot. */}
      <div className="rate-head">
        <span>Riot API{host && <> · {shortHost(host.host)}</>}</span>
        <span>pace <b>{paceMs !== undefined ? fmtPace(paceMs) : '—'}</b></span>
      </div>
      {windows.map((w) => (
        <div key={w.windowSec} className="rate" title={`${w.used} of ${w.limit} requests in the last ${w.windowSec} s`}>
          <span>{w.limit} / {w.windowSec}s</span>
          <Segments fraction={Math.min(1, w.used / w.limit)} count={24} height="h-2" />
          <span>{w.used}</span>
        </div>
      ))}
      <PaceLine view={view} />
      {/* The log-like lines, hidden with Hide details (0.18.0); nothing behind them stops. */}
      {details && <>
        <StatusSlot className="muted small-text">{scraper?.lastEvent}</StatusSlot>
        <StatusSlot className="muted small-text" title="Whose games the Your games tab shows. Settings → Your account sets the server by hand.">
          {scraper?.me ? <>Your account: {scraper.me.riotId ?? 'found'} on {scraper.me.platform} · {num(scraper.me.games)} of your Irelia games stored</>
            : 'Your account: not found yet (open the League client)'}
        </StatusSlot>
        <Hosts hosts={scraper?.hosts ?? []} callsPerMatch={scraper?.matchesScanned ? 1 + (scraper.ireliaGames ?? 0) / scraper.matchesScanned : 2} />
      </>}
      <div className="mt-1 flex justify-end"><DetailsToggle /></div>
    </Card>
  );
}

/** One line per Riot host: each works its own queue at its own pace (server/scraper/sourcing.ts). */
function Hosts({ hosts, callsPerMatch }: { hosts: HostWork[]; callsPerMatch: number }) {
  const rate = useRate();
  if (!hosts.length) return null;
  /**
   * How long a region's queue takes at its pace: each match is one request, and a timeline more for the Irelia games
   * kept (`callsPerMatch`: from the matches and games fetched so far). An estimate: the queue grows as fetching goes on.
   */
  const drain = (h: HostWork) => {
    const pace = rate?.hosts.find((x) => x.host === h.host)?.paceMs ?? rate?.defaultPaceMs;
    return pace && h.queued > 0 ? h.queued * callsPerMatch * pace : null;
  };
  return (
    <>
      <h3>Riot hosts</h3>
      <ul className="host-list">
        {hosts.map((h) => (
          <li key={h.host} className={h.busy ? 'busy' : ''} title={h.lastError ?? `${h.kind === 'platform' ? 'ladders, mastery, ranks' : 'match ids, matches, timelines'} for ${h.platforms.join(', ')}`}>
            <span className="host">{shortHost(h.host)}</span>
            <span className="task">{h.lastError ? `⚠ ${h.task}` : h.task}</span>
            <span className="queued" title={drain(h) !== null ? 'At this pace, for the matches queued now (an estimate: fetching keeps finding more)' : undefined}>
              {h.kind === 'regional' && h.queued > 0 ? `${h.queued.toLocaleString()} queued${drain(h) !== null ? ` · ${fmtAbout(drain(h)!)} (estimate)` : ''}` : ''}
            </span>
          </li>
        ))}
      </ul>
    </>
  );
}

/** Data tab: each region's patch, the games per patch, and what the lane build used and discarded. */
export function PatchesCard({ stats, validation, onChanged }: { stats: DbStats | null; validation: ValidationReport | null; onChanged: () => void }) {
  const inWindow = new Set(stats?.windowPatches ?? []);
  const outside = (stats?.patches ?? []).filter((p) => stats?.windowPatches && !inWindow.has(p.patch));
  // A region's own patch counts: Data Dragon can be a patch ahead before anyone has played on it.
  const isCurrent = (patch: string) => patch === stats?.currentPatch || !!stats?.regions?.some((r) => r.patch === patch);
  const isPrevious = (patch: string) => patch === stats?.previousPatch || !!stats?.regions?.some((r) => r.previousPatch === patch);
  const label = (patch: string) => isCurrent(patch) ? 'current' : isPrevious(patch) ? 'previous patch'
    : inWindow.has(patch) ? 'patch memory' : 'outside the window';
  return (
    <Card title="Patches">
      {!!stats?.regions?.length && (
        <>
          <h3 className="!mt-0">Patch per region</h3>
          <ul className="patch-list regions">
            {[...stats.regions].sort((a, b) => a.platform.localeCompare(b.platform)).map((r) => (
              <li key={r.platform} title={r.builds.map((b) => `${b.gameVersion}: first seen ${new Date(b.firstSeen).toLocaleString()} (${b.games} games)`).join('\n') || undefined}>
                <span>{r.platform}</span>
                <span>{r.patch}{r.fromDataDragon && <span className="muted"> (Data Dragon)</span>}</span>
                <span className="muted">
                  {r.fromDataDragon ? 'no games yet' : `since ${r.startKnown ? '' : '≤ '}${new Date(r.startKnown ? r.start : r.builds[0]?.firstSeen ?? r.start).toLocaleDateString()}`}
                  {r.hotfixes.length > 0 && ` · ${r.hotfixes.length} hotfix${r.hotfixes.length === 1 ? '' : 'es'}`}
                </span>
              </li>
            ))}
          </ul>
        </>
      )}

      <h3>Games per patch</h3>
      <ul className="patch-list">
        {stats?.patches.map((p) => (
          <li key={p.patch} className={isCurrent(p.patch) ? 'fresh' : isPrevious(p.patch) || inWindow.has(p.patch) ? 'prev' : 'stale'}>
            <span>{p.patch}</span>
            <span title={p.lanes ? Object.entries(p.lanes).map(([pos, n]) => `${pos}: ${n}`).join('\n') : undefined}>
              {p.games} games{p.lanes && <span className="muted"> · {p.lanes.TOP ?? 0} top / {p.lanes.MIDDLE ?? 0} mid</span>}
            </span>
            <span>{label(p.patch)}</span>
          </li>
        ))}
      </ul>
      {validation && (
        <div className="validation">
          <span>Lane build used: {validation.freshGames} fresh{validation.previousPatchGames ? ` + ${validation.previousPatchGames} prev-patch` : ''}</span>
          <span>Discarded: {validation.staleDiscarded} stale, {validation.invalidDiscarded} invalid</span>
          {Object.entries(validation.invalidReasons).map(([r, n]) => <span key={r} className="muted">· {r} ×{n}</span>)}
          {validation.conflictsRejected.map((c) => <span key={c} className="conflict">⊘ {c}</span>)}
        </div>
      )}
      <ArchiveSwitch outside={outside} onChanged={onChanged} />
    </Card>
  );
}

/**
 * Data → Patches: the archive switch (server/oldPatches.ts). On, matches of patches older than the six-patch window
 * are compressed into the archive, now and as patches leave the window; off restores them. Nothing is lost.
 */
function ArchiveSwitch({ outside, onChanged }: { outside: { patch: string; games: number }[]; onChanged: () => void }) {
  const { status, pending, error, set } = useOldPatches(onChanged);
  const estimates = useEstimates(`${status?.archived.length ?? 0}|${status?.working ?? ''}|${outside.length}`);
  if (!status) return null;
  const on = pending?.archive ?? status.archive;
  const working = status.working === 'archiving' || status.working === 'restoring' ? status.working : null;
  const archived = status.archived.reduce((s, a) => s + a.games, 0);
  const size = fmtBytes(status.archived.reduce((s, a) => s + a.bytes, 0));
  const waiting = outside.reduce((s, p) => s + p.games, 0);
  const hint = pending?.archive === true || status.working === 'archiving' ? 'Archiving…'
    : pending?.archive === false || status.working === 'restoring' ? 'Restoring…'
      : on
        ? `${archived ? `${archived.toLocaleString('en-US')} matches of ${status.archived.map((a) => a.patch).join(', ')} archived (${size}). ` : ''}Patches are archived as they leave the window; off restores them.`
        : `Off: patches older than the window stay as they are${waiting ? ` (${outside.map((p) => p.patch).join(', ')}: ${waiting.toLocaleString('en-US')} games)` : ''}. On compresses them to about a third of the space, for later; nothing is lost.`;
  return (
    <div className="mt-3 border-t border-ink-600 pt-2">
      <SwitchRow label="Archive patches older than the window" hint={<>{hint}{!working && pending === null && (
        <> <Takes ms={on ? estimates.restore : waiting ? estimates.archive : null} /></>
      )}</>} checked={on} onChange={(v) => set({ archive: v })}
        disabled={pending !== null || status.working !== null} />
      {working && status.progress && (
        <ProgressBar title={working === 'archiving' ? 'Archiving old patches' : 'Restoring the archive'} progress={status.progress}
          label={working === 'archiving' ? 'Archiving patches older than the window…' : 'Restoring the archive…'} />
      )}
      {error && <div className="error">{error}</div>}
    </div>
  );
}

/** Data tab: rebuild from the disk cache and the data audit. Neither calls the Riot API. */
export function ToolsCard({ backfill, jobs }: { backfill: BackfillStatus | null; jobs: JobsStatus | null }) {
  return (
    <Card title="Maintenance">
      <Backfill status={backfill} />
      <Audit job={jobs?.running.find((j) => j.job === 'audit') ?? null} />
    </Card>
  );
}

/** The read-only data audit (npm run audit:data in the app): run it, then copy the report for Claude or keep the file. */
function Audit({ job }: { job: JobsStatus['running'][number] | null }) {
  const [state, setState] = useState<{ running: boolean; text?: string; file?: string; error?: string; copied?: boolean }>({ running: false });
  const estimates = useEstimates(state.file ?? '');
  const run = () => {
    setState({ running: true });
    api.audit().then((r) => setState({ running: false, ...r }), (e) => setState({ running: false, error: e instanceof Error ? e.message : String(e) }));
  };
  const copy = () => { if (state.text) void navigator.clipboard.writeText(state.text).then(() => setState((s) => ({ ...s, copied: true }))); };
  return (
    <div className="audit">
      {job ? (
        <ProgressBar title="Data audit progress" label={job.label ?? 'Starting the audit…'}
          progress={job.progress ?? { fraction: null, etaMs: null, startedAt: job.startedAt, at: job.startedAt }}
          aside={job.step && job.steps ? `step ${job.step} of ${job.steps}` : undefined} />
      ) : (
        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <button type="button" className="ghost" onClick={run} disabled={state.running}
            title="Measures your stored data: coverage, remakes, leavers, lane opponents, what harvesting would add, the yield model, and how each fix changes the lane builds. Read-only, no API calls; runs in its own thread.">
            {state.running ? 'Auditing your data…' : 'Run data audit'}
          </button>
          {!state.running && <Takes ms={estimates.audit} />}
        </div>
      )}
      {state.error && <div className="error">{state.error}</div>}
      {state.text && (
        <>
          <textarea readOnly value={state.text} rows={8} className="audit-text" onFocus={(e) => e.currentTarget.select()} />
          <div className="audit-actions">
            <button type="button" className="ghost" onClick={copy}>{state.copied ? 'Copied' : 'Copy report'}</button>
            {desktop && state.file && <button type="button" className="ghost" onClick={() => void desktop!.openFolder(state.file!.replace(/[\\/][^\\/]+$/, ''))}>Open folder</button>}
          </div>
          <div className="muted small-text">Saved as {state.file}</div>
        </>
      )}
    </div>
  );
}

/** Rebuilding participants, quality flags and timelines from the disk cache (no API calls). */
function Backfill({ status }: { status: BackfillStatus | null }) {
  const [asked, setAsked] = useState(false);
  const running = !!status && status.finishedAt === null && !status.phase.startsWith('Failed');
  const run = () => { setAsked(true); void api.backfill(false).finally(() => setTimeout(() => setAsked(false), 3000)); };
  return (
    <div className="backfill">
      {status && running && (
        <ProgressBar title="Rebuild from cache progress" label={`Rebuilding from cache: ${status.phase}`}
          progress={status.progress ?? { fraction: null, etaMs: null, startedAt: status.startedAt, at: status.startedAt }}
          aside={status.total ? `${status.done.toLocaleString()} / ${status.total.toLocaleString()} matches` : undefined} />
      )}
      {status && !running && (
        <div className="muted small-text">
          {<>{status.phase.startsWith('Failed') ? status.phase : `Cache rebuilt: ${status.done.toLocaleString()} matches, ${status.games.toLocaleString()} Irelia timelines`}
              {status.harvested > 0 && `, ${status.harvested} harvested games queued`}{status.missing > 0 && `, ${status.missing} no longer on disk`}</>}
        </div>
      )}
      <button type="button" className="ghost" onClick={run} disabled={running || asked}
        title="Re-reads every stored match from the disk cache: participants, remakes, leavers, timelines, lane opponents. No API calls.">
        Rebuild from cache
      </button>
    </div>
  );
}
