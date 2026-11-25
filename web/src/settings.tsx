// Settings and the first run: the API key (Irelia Companion's, or pasted; only ever shown masked), regions and rank,
// the disk budget, tier A champions, GPU and learning, and rolling the model back.
import { useEffect, useState } from 'react';
import { api, useGet } from './api.ts';
import { Card, ChampionIcon, ChampionPicker, ErrorText, Toggle } from './components.tsx';
import { useDd } from './ddragon.ts';
import { bytes, gb, looksLikeKey } from './format.ts';
import { PLATFORMS, type ModelStatus, type AppSettings, type ExportReport, type ImportSummary, type KeyView, type Platform, type SettingsView, type StorageStatus } from './types.ts';
import { RollbackButtons, StorageCard } from './pages.tsx';
import { desktop, type DesktopSettings } from './desktop.ts';

const REGION_NAMES: Record<Platform, string> = { EUN1: 'EU Nordic & East', EUW1: 'EU West', KR: 'Korea' };

export function KeyPanel({ view, onChange }: { view: KeyView; onChange: (k: KeyView) => void }) {
  const [paste, setPaste] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const offer = view.ireliaOffer;
  const run = async (f: () => Promise<KeyView>) => {
    setBusy(true);
    setError(null);
    try { onChange(await f()); setPaste(''); } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  return (
    <div className="space-y-3">
      <p className="text-sm">
        {view.set ? <>Key in use: <span className="font-mono text-fg">{view.masked}</span></> : <span className="text-muted">No key set yet.</span>}
      </p>
      {view.error && (
        <ErrorText>
          Riot refused the key ({view.error}). Development keys expire every 24 hours: get a new one at{' '}
          <a className="underline" href={view.portal} target="_blank" rel="noreferrer">developer.riotgames.com</a> and paste it below.
        </ErrorText>
      )}
      {offer.state === 'available' && (
        <div className="flex items-center gap-3">
          <button disabled={busy} className="rounded-md bg-accent px-3 py-1.5 text-sm font-semibold text-bg disabled:opacity-40" onClick={() => run(() => api.post<KeyView>('/api/key/import-irelia'))}>
            Use the same key as Irelia Companion
          </button>
          <span className="text-xs text-muted">Reads only the key from Irelia Companion's settings; nothing there is changed.</span>
        </div>
      )}
      {offer.state === 'unreadable' && <p className="text-xs text-muted">Irelia Companion's settings could not be read ({offer.reason}).</p>}
      {offer.state === 'empty' && <p className="text-xs text-muted">Irelia Companion is installed but has no key set.</p>}
      <form className="flex flex-wrap items-center gap-2" onSubmit={(e) => { e.preventDefault(); if (looksLikeKey(paste)) run(() => api.post<KeyView>('/api/key', { key: paste.trim() })); }}>
        <input
          type="password"
          autoComplete="off"
          spellCheck={false}
          placeholder="Paste a new key (RGAPI-…)"
          value={paste}
          onChange={(e) => setPaste(e.target.value)}
          className="w-80 rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm text-fg outline-none placeholder:text-muted"
        />
        <button disabled={busy || !looksLikeKey(paste)} className="rounded-md border border-line px-3 py-1.5 text-sm text-fg disabled:opacity-40">Paste a new key</button>
        <a className="text-xs text-accent hover:underline" href={view.portal} target="_blank" rel="noreferrer">Get a key</a>
      </form>
      {error && <ErrorText>{error}</ErrorText>}
    </div>
  );
}

function Regions({ value, onChange }: { value: Platform[]; onChange: (v: Platform[]) => void }) {
  return (
    <div className="flex flex-wrap gap-4">
      {PLATFORMS.map((p) => (
        <Toggle key={p} on={value.includes(p)} label={`${p} · ${REGION_NAMES[p]}`} onChange={(on) => {
          const next = on ? [...value, p] : value.filter((x) => x !== p);
          if (next.length) onChange(PLATFORMS.filter((x) => next.includes(x)));
        }} />
      ))}
    </div>
  );
}

function useSettings() {
  const r = useGet<SettingsView>('/api/settings');
  const [view, setView] = useState<SettingsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => { if (r.data) setView(r.data); }, [r.data]);
  const patch = async (p: Partial<AppSettings>) => {
    try { setView(await api.patch<SettingsView>('/api/settings', p)); setError(null); } catch (e) { setError((e as Error).message); }
  };
  return { view, setView, patch, error: error ?? r.error };
}

export function SettingsPage() {
  const dd = useDd();
  const { view, setView, patch, error } = useSettings();
  const storage = useGet<StorageStatus>('/api/storage', 60_000);
  const model = useGet<ModelStatus>('/api/model');
  const [modelView, setModelView] = useState<ModelStatus | null>(null);
  useEffect(() => { if (model.data) setModelView(model.data); }, [model.data]);
  if (!view) return error ? <ErrorText>{error}</ErrorText> : <p className="text-sm text-muted">Loading…</p>;
  const s = view.settings;
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card title="Riot API key" className="lg:col-span-2">
        <KeyPanel view={view.key} onChange={(k) => setView({ ...view, key: k })} />
      </Card>
      {error && <div className="lg:col-span-2"><ErrorText>{error}</ErrorText></div>}
      <Card title="Regions and rank">
        <div className="space-y-3">
          <Regions value={s.regions} onChange={(regions) => patch({ regions })} />
          <div className="flex items-center gap-3 text-sm">
            <span className="text-muted">Rank floor</span>
            <select className="rounded-md border border-line bg-bg px-2 py-1 text-fg" value={s.rankFloor} onChange={(e) => patch({ rankFloor: e.target.value as AppSettings['rankFloor'] })}>
              <option value="masterPlus">Master and above</option>
              <option value="diamond">Diamond and above</option>
            </select>
          </div>
          <p className="text-xs text-muted">Champion-role pairs with few Master+ games widen to Diamond on their own.</p>
          <div className="flex items-center gap-3 text-sm">
            <span className="text-muted">Timelines for other champions</span>
            <select className="rounded-md border border-line bg-bg px-2 py-1 text-fg" value={String(s.timelineSample)} onChange={(e) => patch({ timelineSample: Number(e.target.value) })}>
              {[0.1, 0.25, 0.5, 1].map((v) => <option key={v} value={String(v)}>{Math.round(v * 100)}%</option>)}
              {![0.1, 0.25, 0.5, 1].includes(s.timelineSample) && <option value={String(s.timelineSample)}>{Math.round(s.timelineSample * 100)}%</option>}
            </select>
          </div>
          <p className="text-xs text-muted">A timeline shows who bought what, when, and what the enemy had built: the model learns item choices only from games with one. Your tier A champions always get it. Each timeline is one more request, so a higher share fetches fewer games per hour.</p>
        </div>
      </Card>
      <Card title="Disk budget">
        <div className="flex items-center gap-3 text-sm">
          <input type="range" min={1} max={20} step={1} value={Math.round(s.diskBudgetBytes / gb(1))} onChange={(e) => patch({ diskBudgetBytes: gb(Number(e.target.value)) })} className="w-56" />
          <span className="text-fg">{bytes(s.diskBudgetBytes)}</span>
        </div>
        <p className="mt-1 text-xs text-muted">Over the budget, the oldest patches' games are trimmed; counts and the model stay.</p>
      </Card>
      {storage.data && <StorageCard s={storage.data} />}
      <BackupCard />
      <Card title="Tier A champions">
        <p className="mb-2 text-xs text-muted">Tier A champions get full games with timelines and the learned model. Your most played are added on their own.</p>
        <ul className="mb-3 flex flex-wrap gap-2">
          {s.tierA.map((c) => (
            <li key={c} className="flex items-center gap-1 rounded-full bg-raised py-0.5 pl-0.5 pr-2 text-sm">
              <ChampionIcon id={c} size={22} /> {dd.championName(c)}
              <button aria-label={`Remove ${dd.championName(c)}`} className="ml-1 text-muted hover:text-bad" onClick={() => patch({ tierA: s.tierA.filter((x) => x !== c) })}>×</button>
            </li>
          ))}
          {s.tierADetected.filter((c) => !s.tierA.includes(c)).map((c) => (
            <li key={c} className="flex items-center gap-1 rounded-full border border-line py-0.5 pl-0.5 pr-2 text-sm text-muted" title="Detected from your games">
              <ChampionIcon id={c} size={22} /> {dd.championName(c)}
            </li>
          ))}
        </ul>
        <ChampionPicker value={null} placeholder="Add a champion…" onChange={(c) => { if (!s.tierA.includes(c)) patch({ tierA: [...s.tierA, c] }); }} />
      </Card>
      <Card title="Learning">
        <div className="space-y-3">
          <Toggle on={s.fetching} onChange={(fetching) => patch({ fetching })} label="Fetch new games" hint="Off pauses fetching until you turn it on again, also after a restart. It pauses on its own in champ select and in game." />
          <Toggle on={s.learning} onChange={(learning) => patch({ learning })} label="Learn from new games" hint="Learns each new game as it arrives, a few milliseconds each, except in champ select or a game. The first model waits for an idle PC." />
          <Toggle on={s.gpu} onChange={(gpu) => patch({ gpu })} label="Use the GPU when available" hint="NVIDIA only, off by default. Checked against the CPU; any mismatch falls back to the CPU. Takes effect when Build Fetcher restarts." />
        </div>
      </Card>
      {modelView && modelView.checkpoints.length > 0 && (
        <Card title="Roll back model" className="lg:col-span-2"><RollbackButtons model={modelView} onDone={setModelView} /></Card>
      )}
      {desktop && <DesktopCard />}
    </div>
  );
}

/** The desktop app's own settings (overlay, tray, keeping the PC awake), through window.buildFetcher. */
function DesktopCard() {
  const [s, setS] = useState<DesktopSettings | null>(null);
  useEffect(() => { desktop?.settings.get().then(setS, () => {}); }, []);
  if (!s || !desktop) return null;
  const set = (p: unknown) => desktop!.settings.set(p).then(setS, () => {});
  return (
    <Card title="Desktop">
      <div className="space-y-3">
        <Toggle on={s.overlay.autoShow} onChange={(v) => set({ overlay: { autoShow: v } })} label="Show the overlay when a game starts" />
        <Toggle on={s.overlay.clickThrough} onChange={(v) => set({ overlay: { clickThrough: v } })} label="Overlay is click-through" hint={`${s.hotkeys.toggleOverlay} shows or hides it; ${s.hotkeys.toggleClickThrough} switches click-through (in game only).`} />
        <Toggle on={s.closeToTray} onChange={(v) => set({ closeToTray: v })} label="Closing the window keeps the app in the tray" />
        <Toggle on={s.keepAwakeWhileFetching} onChange={(v) => set({ keepAwakeWhileFetching: v })} label="Keep the PC awake while fetching" />
      </div>
    </Card>
  );
}

/** First run: the key, then the regions; fetching then starts in the background. */
export function FirstRun({ onDone }: { onDone: () => void }) {
  const { view, setView, patch, error } = useSettings();
  const [step, setStep] = useState<'key' | 'region'>('key');
  const [finishError, setFinishError] = useState<string | null>(null);
  if (!view) return error ? <ErrorText>{error}</ErrorText> : <p className="text-sm text-muted">Loading…</p>;
  const finish = async () => {
    try {
      await api.patch<SettingsView>('/api/settings', { firstRunComplete: true });
      onDone();
    } catch (e) {
      setFinishError((e as Error).message);
    }
  };
  return (
    <div className="mx-auto max-w-2xl space-y-4 py-8">
      <h1 className="text-2xl font-semibold text-fg">Welcome to Build Fetcher</h1>
      <p className="text-sm text-muted">Builds for any champion, learned on this PC from ranked games. Two steps, then fetching starts in the background.</p>
      {step === 'key' ? (
        <Card title="1. Your Riot API key">
          <p className="mb-3 text-xs text-muted">Games are fetched with your own key. It stays on this PC and is only ever shown masked.</p>
          <KeyPanel view={view.key} onChange={(k) => setView({ ...view, key: k })} />
          <div className="mt-4 flex justify-end">
            <button disabled={!view.key.set} onClick={() => setStep('region')} className="rounded-md bg-accent px-4 py-1.5 text-sm font-semibold text-bg disabled:opacity-40">Next</button>
          </div>
        </Card>
      ) : (
        <Card title="2. Regions">
          <p className="mb-3 text-xs text-muted">Where games are fetched from. Your own games are always fetched first.</p>
          <Regions value={view.settings.regions} onChange={(regions) => patch({ regions })} />
          {(error ?? finishError) && <ErrorText>{error ?? finishError}</ErrorText>}
          <div className="mt-4 flex justify-between">
            <button onClick={() => setStep('key')} className="text-sm text-muted hover:text-fg">Back</button>
            <button onClick={finish} className="rounded-md bg-accent px-4 py-1.5 text-sm font-semibold text-bg">Start fetching</button>
          </div>
        </Card>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- Storage → Export / Import

/** build-fetcher-backup-2026-11-25.zip */
function backupName(): string {
  return `build-fetcher-backup-${new Date().toISOString().slice(0, 10)}.zip`;
}

/**
 * Export everything learned to one zip, or import one from another PC. An import is checked, then staged: it replaces
 * this PC's data at the next start (the replaced data is kept aside in before-import until the next import).
 */
function BackupCard() {
  const pending = useGet<ImportSummary | null>('/api/backup/pending');
  const [staged, setStaged] = useState<ImportSummary | null>(null);
  const [includeKey, setIncludeKey] = useState(false);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => { setStaged(pending.data ?? null); }, [pending.data]);

  const exportNow = async () => {
    setError(null); setDone(null);
    // In the app: the save dialog. In a plain browser (development): the server's Downloads folder.
    let path: string | null | undefined;
    if (desktop?.backup) {
      path = await desktop.backup.pickSave(backupName());
      if (!path) return;
    }
    setBusy(true);
    try {
      const r = await api.post<ExportReport>('/api/backup/export', { path, includeKey });
      setDone(`Saved ${bytes(r.bytes)} to ${r.path}${r.includesKey ? ' (with your API key: keep the file private)' : ''}.`);
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };

  const importNow = async () => {
    setError(null); setDone(null);
    const path = desktop?.backup ? await desktop.backup.pickOpen() : prompt('Full path of the backup .zip to import:');
    if (!path) return;
    if (!confirm('Importing replaces this PC\'s games, counts, model and settings when Build Fetcher next starts. The current data is kept aside (in before-import) until the next import. Continue?')) return;
    setBusy(true);
    try { setStaged(await api.post<ImportSummary>('/api/backup/import', { path })); } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };

  const cancel = async () => {
    try { await api.post('/api/backup/cancel'); setStaged(null); } catch (e) { setError((e as Error).message); }
  };

  return (
    <Card title="Export and import">
      <p className="mb-3 text-xs text-muted">One zip with your settings, games, counts and model, to move Build Fetcher to another PC or keep a copy. Data Dragon and logs are left out.</p>
      <div className="space-y-3">
        <Toggle on={includeKey} onChange={setIncludeKey} label="Include my Riot API key" hint="Off: the other PC keeps its own key, or asks for one. On: anyone with the file can use your key." />
        <div className="flex flex-wrap gap-2">
          <button disabled={busy} onClick={exportNow} className="rounded-md bg-accent px-3 py-1.5 text-sm font-semibold text-bg disabled:opacity-40">Export…</button>
          <button disabled={busy} onClick={importNow} className="rounded-md border border-line px-3 py-1.5 text-sm text-fg disabled:opacity-40">Import…</button>
        </div>
        {busy && <p className="text-xs text-muted">Working… (a large database can take a minute)</p>}
        {done && <p className="text-sm text-good">{done}</p>}
        {staged && (
          <div className="rounded-md border border-accent/40 bg-accent/10 p-3 text-sm">
            <p>
              Ready to import a backup made by Build Fetcher {staged.version}
              {staged.createdMs > 0 && ` on ${new Date(staged.createdMs).toLocaleString()}`}: {bytes(staged.databaseBytes)} of data, {staged.checkpointFiles} model files
              {staged.includesKey ? ', with an API key' : ', without an API key (this PC keeps its own)'}.
            </p>
            <p className="mt-1 text-xs text-muted">It takes effect when Build Fetcher restarts.</p>
            <div className="mt-2 flex gap-2">
              {desktop?.restart && <button onClick={() => desktop?.restart()} className="rounded-md bg-accent px-3 py-1 text-sm font-semibold text-bg">Restart now</button>}
              <button onClick={cancel} className="rounded-md border border-line px-3 py-1 text-sm">Cancel import</button>
            </div>
          </div>
        )}
        {error && <ErrorText>{error}</ErrorText>}
      </div>
    </Card>
  );
}
