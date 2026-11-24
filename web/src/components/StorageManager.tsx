import { useCallback, useEffect, useState } from 'react';
import type { ConversionStatus, MigrationMode, SlimManifest, StorageAction, StorageInspection, StorageProgress, StorageStats } from '../../../shared/types.ts';
import { api, fmtBytes, useConversion, useEstimates, useOldPatches, useSlim } from '../api.ts';
import { desktop } from '../desktop.ts';
import { SwitchRow } from './common.tsx';
import { clock, fmtAbout, ProgressBar, Takes, useNow } from './Progress.tsx';

const TONE: Record<string, string> = {
  database: 'bg-blood-500',
  riotCache: 'bg-blood-300',
  ddragon: 'bg-gold-400',
  other: 'bg-ash-600',
  backup: 'bg-gold-900',
};

type Notice = { tone: 'ok' | 'err'; text: string } | null;

/** An action button; `takes`: how long it should take (an estimate, shown after the label, when there is one). */
function Btn({ children, onClick, disabled, danger, takes }: {
  children: React.ReactNode; onClick: () => void; disabled?: boolean; danger?: boolean; takes?: number | null;
}) {
  const est = takes !== null && takes !== undefined && Number.isFinite(takes) ? fmtAbout(takes) : null;
  return (
    <button type="button" onClick={onClick} disabled={disabled} title={est ? `Takes ${est} (estimate)` : undefined}
      className={`rounded-md border px-3 py-1.5 text-xs font-semibold transition-colors disabled:opacity-40 ${danger
        ? 'border-blood-700 text-blood-200 hover:bg-blood-900'
        : 'border-ink-600 text-ash-200 hover:border-blood-500 hover:text-ash-50'}`}>
      {children}{est && <span className="ml-1.5 font-normal text-ash-600">· {est}</span>}
    </button>
  );
}

/** "This takes about 3 min (estimate)." for a confirmation, or nothing. */
const takesLine = (ms: number | null | undefined) => (ms !== null && ms !== undefined && Number.isFinite(ms) ? `\n\nThis takes ${fmtAbout(ms)} (estimate).` : '');

/** The background conversion of earlier versions' data (0.14.0): how far it is, or what stopped it. */
function ConversionLine({ status }: { status: ConversionStatus | null }) {
  if (!status || (status.done && !status.running && !status.cache && !status.timelines)) return null;
  const parts: string[] = [];
  if (status.cache && status.cache.total) parts.push(`API cache ${status.cache.done.toLocaleString()} of ${status.cache.total.toLocaleString()} files${status.cache.skipped ? ` (${status.cache.skipped} unreadable, kept)` : ''}`);
  if (status.timelines) parts.push(`timelines ${Math.round(status.timelines.fraction * 100)}%`);
  const state = status.error ? `stopped: ${status.error}` : status.done ? 'done' : status.paused ? 'paused during the game' : status.running ? 'running' : 'waiting';
  return (
    <p className={`mt-2 text-xs ${status.error ? 'text-blood-300' : 'text-ash-400'}`}>
      Converting stored data to the smaller format ({state}){parts.length ? `: ${parts.join(', ')}` : ''}. Nothing is lost: each item is read back and compared before its old copy goes.
    </p>
  );
}

/** What "Keep the slimmer data" gives up, for its confirmation. */
function keepWarning(m: SlimManifest) {
  const p = m.plan;
  return [
    'Delete the backup and keep the slimmer data?',
    '',
    'Without the backup, these can only come back by fetching them from Riot again (API budget):',
    p && p.timelines ? `• the raw timelines of ${p.timelines.toLocaleString()} games older than the newest two patches (their captures stay in the database);` : null,
    `• the fields of cached matches and timelines no capture reads today: a later version that reads more of them would fetch those matches again;`,
    p && p.archivedMatches ? `• in the archive, the other players' kills, items, gold and similar columns of ${p.archivedMatches.toLocaleString()} matches (their per-champion sums are kept).` : null,
  ].filter((x) => x !== null).join('\n');
}

/** Slim stored data's steps while it runs, numbered for the card. */
const SLIM_STEPS: Partial<Record<string, number>> = { backup: 1, baseline: 2, apply: 3, verify: 4 };
const num = (n: number) => n.toLocaleString('en-US');

/** Slim stored data (0.14.0): a backup, the same results checked, then Roll back or Keep. */
function SlimSection({ disabled, onDone }: { disabled: boolean; onDone: () => void }) {
  const [status, setStatus] = useSlim();
  const [error, setError] = useState<string | null>(null);
  const act = (action: 'start' | 'stop' | 'rollback' | 'keep') => {
    setError(null);
    api.slimAction(action).then((s) => { setStatus(s); onDone(); }, (e) => setError(e instanceof Error ? e.message : String(e)));
  };
  const m = status?.manifest ?? null;
  const running = status?.running ?? false;
  const verified = !running && m?.step === 'verified';
  const now = useNow(running);
  // 0.14.2: the phase running, its count and its time left (an estimate); the run's own time below.
  const stepNo = status?.step ? SLIM_STEPS[status.step] : undefined;
  const count = status?.count;
  const aside = [stepNo ? `Step ${stepNo} of 4` : null, count ? `${num(count.done)} of ${num(count.total)} ${count.unit}` : null].filter(Boolean).join(' · ');
  return (
    <div className="mt-3 rounded-md border border-ember bg-ink-900 p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm text-ash-200">Slim stored data</span>
        {!verified && <Btn disabled={disabled || running} onClick={() => act('start')}>{running ? 'Running…' : 'Slim stored data'}</Btn>}
        {running && status?.step !== 'rollback' && !status?.stopping && (
          <Btn onClick={() => { if (window.confirm('Stop Slim stored data? What it did so far is undone (from the slimming on, the backup is put back).')) act('stop'); }}>Stop</Btn>
        )}
      </div>
      <p className="mt-1 text-[11px] text-ash-600">
        Deletes the raw timelines of games older than the newest two patches (their captures stay), the fields of cached matches and timelines nothing reads, and the unread columns of other players in the archive. First a backup, then the same evaluation and 200 drafts before and after: any difference rolls it back by itself. Fetching pauses while it runs; never in champ select or a game.
      </p>
      {running && status?.label && (
        <div className="mt-2 space-y-1">
          <ProgressBar title="Slim stored data" label={status.label} aside={aside || undefined} uncounted="no estimate for this part"
            progress={status.progress ?? { fraction: status.part, etaMs: null, startedAt: m?.startedAt ?? now, at: now }} />
          {m?.startedAt && <p className="text-[11px] tabular-nums text-ash-600">{clock(now - m.startedAt)} in all</p>}
        </div>
      )}
      {verified && m && (
        <div className="mt-2 space-y-2">
          <p className="text-xs text-[#2dbf8d]">Verified: same results. Backup kept: {fmtBytes(m.backupBytes ?? 0)}{m.freedBytes ? ` · ${fmtBytes(m.freedBytes)} freed` : ''}.</p>
          <div className="flex flex-wrap gap-2">
            <Btn disabled={disabled} onClick={() => { if (window.confirm('Put the data back as it was before slimming? Games fetched since stay.')) act('rollback'); }}>Roll back</Btn>
            <Btn danger disabled={disabled} onClick={() => { if (window.confirm(keepWarning(m))) act('keep'); }}>Keep the slimmer data</Btn>
          </div>
        </div>
      )}
      {!running && m && m.step !== 'verified' && m.note && <p className={`mt-2 text-xs ${m.step === 'rolled-back' ? 'text-gold-400' : 'text-ash-400'}`}>{m.note}</p>}
      {(error ?? status?.error) && <p className="mt-2 text-xs text-blood-300">{error ?? status?.error}</p>}
    </div>
  );
}

/** Disk usage of the data folder, cache cleanup, and moving the folder to another drive. */
export function StorageManager({ progress, running }: { progress: StorageProgress | null; running: StorageAction | null }) {
  const [stats, setStats] = useState<StorageStats | null>(null);
  const [action, setAction] = useState<string | null>(null);   // request in flight
  const [notice, setNotice] = useState<Notice>(null);
  const [target, setTarget] = useState('');
  const [check, setCheck] = useState<StorageInspection | null>(null);
  const [mode, setMode] = useState<MigrationMode>('move');
  const old = useOldPatches(() => void load());
  const conversion = useConversion(stats?.conversion);
  // Deleting old patches' games: its estimate comes with the others the server works out (the rest with the stats).
  const other = useEstimates(`${stats?.totalBytes ?? 0}|${action ?? ''}`);

  const load = useCallback(() => api.storageStats().then(setStats).catch((e) => setNotice({ tone: 'err', text: String(e.message ?? e) })), []);
  useEffect(() => { void load(); }, [load]);

  // Pre-flight the typed / picked folder (debounced) and default the mode to what the folder allows.
  useEffect(() => {
    if (!target.trim()) { setCheck(null); return; }
    const t = setTimeout(() => {
      api.inspectFolder(target).then((c) => { setCheck(c); setMode((m) => (c.hasData ? 'adopt' : m === 'adopt' ? 'move' : m)); }).catch(() => setCheck(null));
    }, 300);
    return () => clearTimeout(t);
  }, [target]);

  const run = async (name: string, fn: () => Promise<string>) => {
    setAction(name); setNotice(null);
    try { setNotice({ tone: 'ok', text: await fn() }); } catch (e) { setNotice({ tone: 'err', text: e instanceof Error ? e.message : String(e) }); }
    finally { setAction(null); void load(); }
  };

  const clear = (target: Parameters<typeof api.clearStorage>[0], label: string) => run(target, async () => {
    const r = await api.clearStorage(target);
    return `${label}: freed ${fmtBytes(r.freedBytes)}${r.files ? ` (${r.files.toLocaleString()} files)` : ''}.`;
  });

  const migrate = () => run('migrate', async () => {
    const r = await api.migrateStorage(target, mode);
    setTarget(''); setCheck(null);
    const left = r.leftovers.length ? ` Left in the old folder (not ours): ${r.leftovers.join(', ')}.` : '';
    return mode === 'adopt' ? `Now using the data in ${r.dataDir}.` : `${mode === 'move' ? 'Moved' : 'Copied'} ${fmtBytes(r.copiedBytes)} to ${r.dataDir}.${left}`;
  });

  const browse = async () => {
    const dir = await desktop?.selectDirectory(stats?.dataDir);
    if (dir) setTarget(dir);
  };

  if (!stats) return <p className="text-sm text-ash-400">{notice?.text ?? 'Measuring the data folder…'}</p>;

  const moving = action === 'migrate' || stats.busy;
  const ratio = progress && progress.totalBytes ? progress.doneBytes / progress.totalBytes : null;
  const disk = stats.disk;

  return (
    <div className="space-y-4">
      {/* usage */}
      <div>
        <div className="flex items-baseline justify-between">
          <span className="text-2xl font-extrabold tabular-nums text-ash-50">{fmtBytes(stats.totalBytes)}</span>
          {disk && <span className="text-xs text-ash-400">{fmtBytes(disk.freeBytes)} free of {fmtBytes(disk.totalBytes)} on this drive</span>}
        </div>
        <div className="mt-2 flex h-2.5 overflow-hidden rounded-full bg-ink-950" role="img"
          aria-label={stats.categories.map((c) => `${c.label} ${fmtBytes(c.bytes)}`).join(', ')}>
          {stats.categories.filter((c) => c.bytes > 0).map((c) => (
            <div key={c.key} className={TONE[c.key]} style={{ width: `${Math.max(1, (c.bytes / Math.max(1, stats.totalBytes)) * 100)}%` }} />
          ))}
        </div>
        <ul className="mt-3 space-y-1.5">
          {stats.categories.map((c) => (
            <li key={c.key}>
              <details className="group">
                <summary className="flex cursor-pointer list-none items-center gap-2 text-sm">
                  <span className={`size-2.5 rounded-sm ${TONE[c.key]}`} />
                  <span className="text-ash-200">{c.label}</span>
                  <span className="ml-auto tabular-nums text-ash-50">{fmtBytes(c.bytes)}</span>
                  <span className="w-24 text-right text-xs tabular-nums text-ash-600">{c.files.toLocaleString()} files</span>
                </summary>
                {c.key === 'database' && !!stats.tables?.length && (
                  <ul className="mt-1 mb-1 ml-5 space-y-0.5 text-xs text-ash-400" aria-label="Database tables">
                    {stats.tables.map((t) => (
                      <li key={t.name} className="flex justify-between gap-2 font-mono">
                        <span className="truncate">{t.name}</span>
                        <span className="tabular-nums">{t.rows.toLocaleString()} rows{t.bytes !== null && ` · ${fmtBytes(t.bytes)}`}</span>
                      </li>
                    ))}
                  </ul>
                )}
                {c.parts.length > 0 && (
                  <ul className="mt-1 mb-2 ml-5 space-y-0.5 text-xs text-ash-400">
                    {c.parts.map((p) => (
                      <li key={p.name} className="flex justify-between gap-2 font-mono">
                        <span className="truncate">{p.name}{c.key === 'ddragon' && p.name === stats.ddragonVersion ? ' (in use)' : ''}</span>
                        <span className="tabular-nums">{fmtBytes(p.bytes)}</span>
                      </li>
                    ))}
                  </ul>
                )}
              </details>
            </li>
          ))}
        </ul>
      </div>

      {/* cleanup */}
      <div>
        <h4 className="mb-2 text-xs font-semibold uppercase tracking-[0.14em] text-ash-400">Clean up</h4>
        {old.status && (
          <div className="mb-2">
            <SwitchRow label="Keep the API cache to the six-patch window"
              checked={old.pending?.trimCache ?? old.status.trimCache} onChange={(v) => old.set({ trimCache: v })}
              disabled={old.pending !== null || old.status.working !== null || moving}
              hint={old.pending?.trimCache || old.status.working === 'trimming' ? 'Deleting cached files of old patches…'
                : <>{`Deletes the cached matches and timelines of patches older than the window, now and as patches leave it, so the cache stops growing. Their games stay in the database.${old.status.trimCache && old.status.last?.includes('cached') ? ` ${old.status.last}` : ''}`}
                  {!old.status.trimCache && <> <Takes ms={other.trim} /></>}</>} />
            {old.status.working === 'trimming' && old.status.progress && (
              <ProgressBar title="Trimming the API cache" progress={old.status.progress} label="Deleting cached files of patches older than the window…" />
            )}
            {old.error && <div className="text-xs text-blood-300">{old.error}</div>}
          </div>
        )}
        <div className="flex flex-wrap gap-2">
          <Btn disabled={!!action || moving} takes={stats.estimates?.['clear-expired']} onClick={() => void clear('riot-expired', 'Expired API responses')}>Clear expired API cache</Btn>
          <Btn disabled={!!action || moving} takes={stats.estimates?.['ddragon-old']} onClick={() => void clear('ddragon-old', 'Old Data Dragon versions')}>Remove old Data Dragon patches</Btn>
          <Btn disabled={!!action || moving} takes={stats.estimates?.compact} onClick={() => void clear('compact-db', 'Database compacted')}>Compact database</Btn>
          <Btn danger disabled={!!action || moving} takes={stats.estimates?.['clear-cache']} onClick={() => {
            if (window.confirm(`Delete every cached Riot API response? Matches and timelines will be downloaded again when fetching needs them (this uses API budget). Your database of Irelia games is kept.${takesLine(stats.estimates?.['clear-cache'])}`)) {
              void clear('riot-cache', 'Riot API cache cleared');
            }
          }}>Clear entire API cache</Btn>
          <Btn danger disabled={!!action || moving} takes={other.purge} onClick={() => {
            if (window.confirm(`Delete the games of patches older than the six-patch window for good? Their per-champion totals are kept. The archive switch (Data → Patches) keeps them compressed instead.${takesLine(other.purge)}`)) {
              void run('purge', async () => { const { removed } = await api.purgeStale(); return `Deleted ${removed.toLocaleString()} matches of old patches; their per-champion totals are kept.`; });
            }
          }}>Delete games of old patches</Btn>
        </div>
        {running && <div className="mt-2"><ProgressBar title={running.text} progress={running.progress} label={running.text} /></div>}
        <ConversionLine status={conversion} />
        <SlimSection disabled={!!action || moving} onDone={() => void load()} />
        {stats.suggestCompact && stats.suggestCompact.freeBytes > 32 * 1024 * 1024 && (
          <p className="mt-2 text-xs text-gold-400">
            Compact database once: it gives back {fmtBytes(stats.suggestCompact.freeBytes)} now, and from then on the space deleted data leaves comes back by itself.
          </p>
        )}
        <p className="mt-1.5 text-[11px] text-ash-600">Clearing the cache never deletes the Irelia games stored in the database. Deleting games of old patches does (their per-champion totals are kept); the archive switch on the Data tab keeps them compressed instead.</p>
      </div>

      {/* location */}
      <div>
        <h4 className="mb-2 text-xs font-semibold uppercase tracking-[0.14em] text-ash-400">Data location</h4>
        <div className="flex items-center gap-2">
          <code className="min-w-0 flex-1 truncate rounded-md border border-ember bg-ink-950 px-2 py-1.5 text-xs text-ash-200" title={stats.dataDir}>{stats.dataDir}</code>
          {desktop && <Btn onClick={() => void desktop!.openFolder(stats.dataDir)}>Open</Btn>}
        </div>
        <p className="mt-1 text-[11px] text-ash-600">Settings file: <span className="font-mono">{stats.configFile}</span></p>

        {stats.lockedBy ? (
          <p className="mt-2 text-xs text-gold-400">The folder is pinned by the {stats.lockedBy}; moving it is disabled.</p>
        ) : (
          <div className="mt-3 space-y-2 rounded-md border border-ember bg-ink-900 p-3">
            <label className="block text-xs text-ash-400" htmlFor="data-target">Change data location</label>
            <div className="flex gap-2">
              <input id="data-target" value={target} onChange={(e) => setTarget(e.target.value)} disabled={moving}
                placeholder="D:\Games\IreliaData" spellCheck={false}
                className="min-w-0 flex-1 rounded-md border border-ink-600 bg-ink-950 px-2 py-1.5 font-mono text-xs text-ash-50 outline-none focus:border-blood-500" />
              {desktop && <Btn disabled={moving} onClick={() => void browse()}>Browse…</Btn>}
            </div>

            {check && (
              <div className="space-y-2 text-xs">
                {check.problems.map((p) => <div key={p} className="text-blood-300">✖ {p}</div>)}
                {!check.problems.length && (
                  <>
                    <div className="text-ash-400">
                      {check.hasData ? 'This folder already contains Irelia Companion data.' : check.exists ? 'Folder exists and is writable.' : 'The folder will be created.'}
                      {' '}Needs {fmtBytes(check.requiredBytes)}{check.freeBytes !== null && <>, {fmtBytes(check.freeBytes)} free</>}.
                    </div>
                    <fieldset className="flex flex-wrap gap-3">
                      <legend className="sr-only">What to do with your data</legend>
                      {(check.hasData ? ['adopt'] as const : ['move', 'copy', 'adopt'] as const).map((m) => (
                        <label key={m} className="flex items-center gap-1.5 text-ash-200">
                          <input type="radio" name="mode" value={m} checked={mode === m} onChange={() => setMode(m)} className="accent-blood-500" />
                          {m === 'move' ? 'Move (copy, verify, delete old)' : m === 'copy' ? 'Copy (keep old folder)' : check.hasData ? 'Use the existing data there' : 'Start empty there'}
                        </label>
                      ))}
                    </fieldset>
                    <button type="button" onClick={() => void migrate()} disabled={moving}
                      className="rounded-md bg-blood-600 px-4 py-2 font-bold text-ash-50 hover:bg-blood-500 disabled:opacity-40">
                      {mode === 'adopt' ? 'Switch folder' : mode === 'move' ? 'Move data' : 'Copy data'}
                    </button>
                  </>
                )}
              </div>
            )}

            {moving && progress && (
              <ProgressBar title="Moving the data folder" label={progress.text}
                progress={progress.progress ?? { fraction: ratio, etaMs: null, startedAt: Date.now(), at: Date.now() }}
                aside={ratio !== null ? `${fmtBytes(progress.doneBytes)} / ${fmtBytes(progress.totalBytes)}` : undefined} />
            )}
            <p className="text-[11px] text-ash-600">The scraper pauses while files move. Nothing is deleted until the copy is verified (sizes, SQLite integrity, row counts).</p>
          </div>
        )}
      </div>

      {notice && (
        <div role="status" className={`rounded-md border px-3 py-2 text-sm ${notice.tone === 'ok' ? 'border-[#2dbf8d55] text-[#2dbf8d]' : 'border-blood-700 bg-blood-900/40 text-blood-200'}`}>
          {notice.text}
        </div>
      )}
    </div>
  );
}
