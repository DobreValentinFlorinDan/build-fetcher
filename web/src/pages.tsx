// The dashboard's pages: Build (browse any champion), Champ select, Fetching, Model, About.
import { useEffect, useState } from 'react';
import { api, useGet } from './api.ts';
import { BudgetMeter, BuildView, Card, ChampionIcon, ChampionPicker, ErrorText, RolePicker } from './components.tsx';
import { useDd } from './ddragon.ts';
import { ago, bytes, count, dataAge, ROLE_NAMES } from './format.ts';
import type { BuildRecommendation, DraftState, ImportResult, ModelStatus, PairVolume, Role, SettingsView, StorageStatus, Status } from './types.ts';

// ---------------------------------------------------------------- import into the client

export function ImportButton({ champion, role, clientConnected }: { champion: number; role: Role | null; clientConnected: boolean }) {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<ImportResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => { setResult(null); setError(null); }, [champion, role]);
  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      setResult(await api.post<ImportResult>('/api/import', { champion, role }));
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const part = (name: string, p: ImportResult['runePage']) => (
    <li className={p.ok ? 'text-good' : 'text-bad'}>{name}: {p.ok ? (p.replaced ? 'replaced the previous one' : 'added') : p.error}</li>
  );
  return (
    <div className="space-y-2">
      <button
        disabled={busy || !clientConnected}
        onClick={run}
        title={clientConnected ? 'Adds a rune page and an item set to the League client (only pages and sets named "Build Fetcher" are ever replaced)' : 'Open the League client to import'}
        className="rounded-md bg-accent px-3 py-1.5 text-sm font-semibold text-bg disabled:opacity-40"
      >
        {busy ? 'Importing…' : 'Import runes and item set'}
      </button>
      {result && <ul className="text-xs">{part('Rune page', result.runePage)}{part(`Item set "${result.title}"`, result.itemSet)}</ul>}
      {error && <ErrorText>{error}</ErrorText>}
    </div>
  );
}

// ---------------------------------------------------------------- Build

export function BuildPage({ status, initial }: { status: Status | null; initial?: { champion: number; role: Role | null } }) {
  const dd = useDd();
  const [champion, setChampion] = useState<number | null>(initial?.champion ?? null);
  const [role, setRole] = useState<Role | null>(initial?.role ?? null);
  const [vs, setVs] = useState<number | null>(null);
  const [rec, setRec] = useState<BuildRecommendation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  useEffect(() => {
    if (champion == null) return;
    let live = true;
    setLoading(true);
    const q = new URLSearchParams({ champion: String(champion) });
    if (role) q.set('role', role);
    if (vs) q.set('vs', String(vs));
    api.get<BuildRecommendation>(`/api/build?${q}`).then(
      (r) => { if (live) { setRec(r); setError(null); setLoading(false); } },
      (e: Error) => { if (live) { setError(e.message); setLoading(false); } },
    );
    return () => { live = false; };
  }, [champion, role, vs]);
  return (
    <div className="space-y-4">
      <Card>
        <div className="flex flex-wrap items-center gap-3">
          <ChampionPicker value={champion} onChange={setChampion} />
          <RolePicker value={role} onChange={setRole} />
          <span className="text-sm text-muted">vs</span>
          <div className="w-56"><ChampionPicker value={vs} onChange={setVs} placeholder="Any lane opponent" /></div>
          {vs != null && <button className="text-xs text-muted hover:text-fg" onClick={() => setVs(null)}>clear</button>}
        </div>
        {!dd.ready && <p className="mt-2 text-xs text-muted">Loading champion data from Data Dragon…</p>}
      </Card>
      {champion == null && <p className="text-sm text-muted">Pick any champion to see its build: runes, summoner spells, skill order, starting items, item path, boots and late-game swaps, each with several options and the games behind them.</p>}
      {error && <ErrorText>{error}</ErrorText>}
      {loading && !rec && <p className="text-sm text-muted">Working it out…</p>}
      {rec && champion != null && (
        <>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="flex items-center gap-3 text-xl font-semibold text-fg"><ChampionIcon id={rec.champion} size={40} />{dd.championName(rec.champion)} <span className="text-muted">{ROLE_NAMES[rec.role]}</span></h1>
            <ImportButton champion={rec.champion} role={rec.role} clientConnected={status?.lcu.status === 'connected'} />
          </div>
          <BuildView rec={rec} />
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- Champ select

export function ChampSelectPage({ draft, status }: { draft: DraftState | null; status: Status | null }) {
  const dd = useDd();
  if (!draft) {
    return (
      <Card title="Champ select">
        <p className="text-sm text-muted">
          {status?.lcu.status === 'connected' ? 'Waiting for champ select. Your build shows here as soon as you hover a champion, and updates as picks come in.' : 'The League client is not running (or not found). Start it and enter champ select.'}
        </p>
      </Card>
    );
  }
  const cs = draft.champSelect;
  const team = (ally: boolean) => cs.picks.filter((p) => p.ally === ally);
  return (
    <div className="space-y-4">
      <Card title="Draft" right={<span className="text-xs text-muted">Champions and roles only: names are hidden in champ select and never read.</span>}>
        <div className="grid gap-4 md:grid-cols-2">
          {[true, false].map((ally) => (
            <div key={String(ally)}>
              <h3 className="mb-2 text-xs uppercase tracking-wider text-muted">{ally ? 'Your team' : 'Enemy team'}</h3>
              <ul className="space-y-1">
                {team(ally).map((p, i) => (
                  <li key={i} className={`flex items-center gap-2 rounded px-2 py-1 text-sm ${p.me ? 'bg-raised' : ''}`}>
                    {p.champion ? <ChampionIcon id={p.champion} size={28} /> : <span className="h-7 w-7 rounded bg-raised" />}
                    <span className={p.locked ? 'text-fg' : 'text-muted italic'}>{p.champion ? dd.championName(p.champion) : '—'}</span>
                    {p.role && <span className="text-xs text-muted">{ROLE_NAMES[p.role]}</span>}
                    {p.me && <span className="ml-auto text-xs text-accent">you</span>}
                    {!p.locked && p.champion !== 0 && <span className="ml-auto text-[11px] text-muted">hovering</span>}
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>
      </Card>
      {cs.draft.champion === 0 && <p className="text-sm text-muted">Hover a champion to see its build.</p>}
      {draft.error && <ErrorText>{draft.error}</ErrorText>}
      {draft.pending && !draft.recommendation && <p className="text-sm text-muted">Working out the build…</p>}
      {draft.recommendation && (
        <>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="flex items-center gap-3 text-xl font-semibold text-fg">
              <ChampionIcon id={draft.recommendation.champion} size={40} />
              {dd.championName(draft.recommendation.champion)} <span className="text-muted">{ROLE_NAMES[draft.recommendation.role]}</span>
              {draft.pending && <span className="text-xs font-normal text-muted">updating…</span>}
            </h1>
            <ImportButton champion={draft.recommendation.champion} role={draft.recommendation.role} clientConnected={status?.lcu.status === 'connected'} />
          </div>
          <BuildView rec={draft.recommendation} />
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- Fetching

/**
 * Start / stop for fetching or learning. It saves the setting (kept across restarts); the server applies it at once:
 * a paused fetcher cancels its requests in flight, a paused learner stops its update at the next mini-batch.
 */
export function SwitchButton({ field, on, onChange }: { field: 'fetching' | 'learning'; on: boolean; onChange: (on: boolean) => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const flip = async () => {
    setBusy(true);
    try {
      const v = await api.patch<SettingsView>('/api/settings', { [field]: !on });
      onChange(v.settings[field]);
      setError(null);
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  const what = field === 'fetching' ? 'fetching' : 'learning';
  return (
    <span className="inline-flex items-center gap-2">
      <button disabled={busy} onClick={flip} className={`rounded-md px-3 py-1 text-sm font-semibold disabled:opacity-40 ${on ? 'border border-line text-fg' : 'bg-accent text-bg'}`}>
        {on ? `Pause ${what}` : `Resume ${what}`}
      </button>
      {error && <span className="text-xs text-bad">{error}</span>}
    </span>
  );
}

export function FetchingPage({ status }: { status: Status | null }) {
  const dd = useDd();
  const settings = useGet<SettingsView>('/api/settings');
  const [fetching, setFetching] = useState<boolean | null>(null);
  useEffect(() => { if (settings.data) setFetching(settings.data.settings.fetching); }, [settings.data]);
  const volumes = useGet<PairVolume[]>('/api/data-age', 60_000);
  const storage = useGet<StorageStatus>('/api/storage', 60_000);
  const f = status?.fetch;
  const now = Date.now();
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card title="Fetching" right={<span className="inline-flex items-center gap-3">{f && <span className={`text-xs ${f.running ? 'text-good' : 'text-warn'}`}>{f.running ? 'running' : `paused${f.paused ? `: ${f.paused}` : ''}`}</span>}{fetching !== null && <SwitchButton field="fetching" on={fetching} onChange={setFetching} />}</span>}>
        {!f ? <p className="text-sm text-muted">Connecting…</p> : (
          <div className="space-y-4">
            <div className="grid grid-cols-3 gap-3 text-center">
              <Stat label="matches / hour" value={count(f.matchesLastHour)} />
              <Stat label="requests / hour" value={count(f.requestsLastHour)} />
              <Stat label="matches stored" value={count(f.matchesTotal)} />
            </div>
            {f.current && <p className="text-sm text-muted">Now: <span className="text-fg">{f.current}</span></p>}
            {f.keyError && <ErrorText>Riot refused the key ({f.keyError}). Get a new one at <a className="underline" href="https://developer.riotgames.com" target="_blank" rel="noreferrer">developer.riotgames.com</a> and paste it in Settings.</ErrorText>}
            <div className="space-y-3">
              <h3 className="text-xs uppercase tracking-wider text-muted">Requests per routing value</h3>
              {f.budgets.length === 0 && <p className="text-sm text-muted">No answers from Riot yet.</p>}
              {f.budgets.map((b) => <BudgetMeter key={b.scope} b={b} />)}
              <p className="text-[11px] text-muted"><span className="inline-block h-2 w-2 rounded-sm bg-accent" /> Build Fetcher <span className="ml-3 inline-block h-2 w-2 rounded-sm bg-warn" /> another app on the same key. Requests that would exceed Riot's count are never sent.</p>
            </div>
            {f.dropped.length > 0 && (
              <div>
                <h3 className="mb-1 text-xs uppercase tracking-wider text-muted">Games and answers dropped</h3>
                <ul className="flex flex-wrap gap-x-4 text-sm text-muted">{f.dropped.map(([r, n]) => <li key={r}>{r}: <span className="text-fg">{count(n)}</span></li>)}</ul>
              </div>
            )}
          </div>
        )}
      </Card>
      <Card title="Data age per champion">
        {volumes.error && <ErrorText>{volumes.error}</ErrorText>}
        <div className="max-h-[28rem] overflow-auto">
          <table className="w-full text-sm">
            <thead className="sticky top-0 bg-panel text-left text-xs text-muted"><tr><th className="py-1">Champion</th><th>Roles</th><th className="text-right" title="Held-out matches these scores were measured on (re-measured as the test set grows)">Test matches</th><th className="text-right">Newest game</th></tr></thead>
            <tbody>
              {dataAge(volumes.data ?? []).map((r) => (
                <tr key={r.champion} className="border-t border-line">
                  <td className="flex items-center gap-2 py-1"><ChampionIcon id={r.champion} size={22} />{dd.championName(r.champion)}</td>
                  <td className="text-xs text-muted">{r.roles.map((x) => ROLE_NAMES[x]).join(', ')}</td>
                  <td className="text-right">{count(r.games)}</td>
                  <td className="text-right text-muted">{ago(r.newestMs, now)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Card>
      {storage.data && <StorageCard s={storage.data} />}
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return <div className="rounded-md bg-raised p-2"><div className="text-lg font-semibold text-fg">{value}</div><div className="text-[11px] text-muted">{label}</div></div>;
}

export function StorageCard({ s }: { s: StorageStatus }) {
  const share = s.budgetBytes > 0 ? Math.min(1, s.usedBytes / s.budgetBytes) : 0;
  return (
    <Card title="Storage" right={<span className="text-xs text-muted">{bytes(s.usedBytes)} of {bytes(s.budgetBytes)}</span>}>
      <div className="mb-3 h-2 overflow-hidden rounded bg-raised"><div className="h-full bg-accent" style={{ width: `${share * 100}%` }} /></div>
      <ul className="grid grid-cols-2 gap-x-6 gap-y-1 text-sm">{s.parts.map(([p, n]) => <li key={p} className="flex justify-between"><span className="text-muted">{p}</span><span>{bytes(n)}</span></li>)}</ul>
      <p className="mt-2 text-xs text-muted">
        Patches kept: {s.patches.map((p) => `${p.major}.${p.minor}`).join(', ') || '—'}
        {s.lastCleanupFreedBytes != null && ` · last clean-up freed ${bytes(s.lastCleanupFreedBytes)}`}
      </p>
    </Card>
  );
}

// ---------------------------------------------------------------- Model

export function RollbackButtons({ model, onDone }: { model: ModelStatus; onDone: (m: ModelStatus) => void }) {
  const [error, setError] = useState<string | null>(null);
  const roll = async (version: string) => {
    if (!confirm(`Use model ${version} again? Newer checkpoints are kept and can be chosen later.`)) return;
    try { onDone(await api.post<ModelStatus>('/api/model/rollback', { version })); setError(null); } catch (e) { setError((e as Error).message); }
  };
  return (
    <div>
      <table className="w-full text-sm">
        <thead className="text-left text-xs text-muted"><tr><th className="py-1">Version</th><th>Kind</th><th className="text-right">Log loss</th><th className="text-right">Brier</th><th className="text-right">Calibration error</th><th className="text-right">Games</th><th /></tr></thead>
        <tbody>
          {model.checkpoints.map((c) => (
            <tr key={c.version} className="border-t border-line">
              <td className="py-1.5 font-mono">{c.version}{c.active && <span className="ml-2 rounded bg-accent/20 px-1.5 text-xs text-accent">active</span>}</td>
              <td className="text-muted">{c.kind}</td>
              <td className="text-right">{c.metrics.logLoss.toFixed(4)}</td>
              <td className="text-right">{c.metrics.brier.toFixed(4)}</td>
              <td className="text-right text-muted" title="Per 10-minute window: 0-10, 10-20, 20-30, 30+">{c.metrics.calibration.map((x) => x.toFixed(3)).join(' · ')}</td>
              <td className="text-right" title={`${count(c.metrics.games)} examples (a player at a moment of a match)`}>{c.metrics.matches > 0 ? count(c.metrics.matches) : '—'}</td>
              <td className="text-right">{!c.active && <button className="text-xs text-accent hover:underline" onClick={() => roll(c.version)}>Roll back</button>}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {error && <ErrorText>{error}</ErrorText>}
    </div>
  );
}

export function ModelPage() {
  const m = useGet<ModelStatus>('/api/model', 15_000);
  const [model, setModel] = useState<ModelStatus | null>(null);
  useEffect(() => { if (m.data) setModel(m.data); }, [m.data]);
  if (!model) return m.error ? <ErrorText>{m.error}</ErrorText> : <p className="text-sm text-muted">Loading…</p>;
  return (
    <div className="space-y-4">
      <Card title="Model" right={<span className="inline-flex items-center gap-3"><span className="text-xs text-muted">{model.learning ? 'learning on' : 'learning off'} · {model.backend}</span><SwitchButton field="learning" on={model.learning} onChange={(learning) => setModel({ ...model, learning })} /></span>}>
        <p className="mb-3 text-xs text-muted">Each update is checked on held-out games; one that does worse on any metric is rejected and the previous checkpoint stays.</p>
        {model.checkpoints.length === 0 ? <p className="text-sm text-muted">No checkpoint yet: builds come from counts until the first model is trained.</p> : <RollbackButtons model={model} onDone={setModel} />}
      </Card>
      <Card title="Learning log">
        {model.log.length === 0 ? <p className="text-sm text-muted">Nothing yet.</p> : <ul className="space-y-1 font-mono text-xs text-muted">{model.log.map((l, i) => <li key={i}>{l}</li>)}</ul>}
      </Card>
    </div>
  );
}

// ---------------------------------------------------------------- About

export const LEGAL =
  "Build Fetcher isn't endorsed by Riot Games and doesn't reflect the views or opinions of Riot Games or anyone officially involved in producing or managing Riot Games properties. Riot Games, and all associated properties are trademarks or registered trademarks of Riot Games, Inc.";

export function AboutPage({ status }: { status: Status | null }) {
  return (
    <div className="max-w-3xl space-y-4">
      <Card title="Build Fetcher">
        <div className="space-y-2 text-sm text-muted">
          <p>Version {status?.version ?? '—'}. Builds for any champion and role, learned on this PC from ranked games fetched with your own Riot API key.</p>
          <p>Local only: the app talks to the Riot API (with your key), Data Dragon, and the League client and the game on this PC (127.0.0.1). No telemetry, no server of ours.</p>
          <p>In game, the overlay shows several options with the games behind them; it highlights choices and leaves them to you. It doesn't pop up, alert or track timers. It reads nothing from the game's memory and injects nothing.</p>
          <p>In champ select, only champions and roles are read: other players' names stay hidden.</p>
        </div>
      </Card>
      <Card title="Legal"><p className="text-sm text-muted">{LEGAL}</p></Card>
    </div>
  );
}
