// The dashboard: first run until the key and region are set, then the pages. Champ select opens by itself when a draft
// starts (a page change in the app's own window, not a pop-up).
import { useEffect, useMemo, useState } from 'react';
import { useGet, useServer, type ServerState } from './api.ts';
import { Dd, DdContext } from './ddragon.ts';
import { AboutPage, BuildPage, ChampSelectPage, FetchingPage, LEGAL, ModelPage } from './pages.tsx';
import { FirstRun, SettingsPage } from './settings.tsx';
import type { Activity, StaticView } from './types.ts';
import { EMPTY_STATIC } from './ddragon.ts';

type Page = 'build' | 'champSelect' | 'fetching' | 'model' | 'settings' | 'about';
const PAGES: [Page, string][] = [['build', 'Build'], ['champSelect', 'Champ select'], ['fetching', 'Fetching'], ['model', 'Model'], ['settings', 'Settings'], ['about', 'About']];

const ACTIVITY: Record<Activity, string> = { idle: 'PC idle', busy: 'PC in use: learning goes on', champSelect: 'Champ select: learning paused', inGame: 'In game: learning paused' };

function remembered(): Page {
  try {
    const p = localStorage.getItem('bf.page') as Page | null;
    return p && PAGES.some((x) => x[0] === p) ? p : 'build';
  } catch {
    return 'build';
  }
}

export function useStatic(server: ServerState): Dd {
  const s = useGet<StaticView>('/api/static', undefined, [server.staticVersion]);
  return useMemo(() => new Dd(s.data ?? EMPTY_STATIC), [s.data]);
}

export function App() {
  const server = useServer();
  const dd = useStatic(server);
  const [page, setPage] = useState<Page>(remembered);
  const [setupDone, setSetupDone] = useState(false);
  const status = server.status;
  useEffect(() => { try { localStorage.setItem('bf.page', page); } catch { /* private mode */ } }, [page]);
  // A draft starting brings its build forward once; the user can leave the page while it lasts.
  const drafting = server.draft != null;
  useEffect(() => { if (drafting) setPage('champSelect'); }, [drafting]);

  const firstRun = status != null && !status.firstRunComplete && !setupDone;
  return (
    <DdContext.Provider value={dd}>
      <div className="flex min-h-screen flex-col bg-bg text-fg">
        <header className="flex flex-wrap items-center gap-x-6 gap-y-2 border-b border-line bg-panel px-5 py-2.5">
          <span className="text-sm font-bold tracking-[0.3em] text-accent">BUILD FETCHER</span>
          {!firstRun && (
            <nav className="flex flex-wrap gap-1">
              {PAGES.map(([id, label]) => (
                <button key={id} onClick={() => setPage(id)} className={`rounded-md px-3 py-1 text-sm ${page === id ? 'bg-raised text-fg' : 'text-muted hover:text-fg'}`}>
                  {label}
                  {id === 'champSelect' && drafting && <span className="ml-1.5 inline-block h-1.5 w-1.5 rounded-full bg-accent align-middle" />}
                </button>
              ))}
            </nav>
          )}
          <span className="ml-auto flex items-center gap-3 text-xs text-muted">
            {status && <span>{ACTIVITY[status.activity]}</span>}
            <span title="The League client">client: {server.lcu?.status ?? '—'}</span>
            <span className={`inline-block h-2 w-2 rounded-full ${server.connected ? 'bg-good' : 'bg-bad'}`} title={server.connected ? 'Connected to the app' : 'Not connected to the app'} />
          </span>
        </header>
        <main className="flex-1 p-5">
          {status == null ? (
            <p className="text-sm text-muted">{server.connected ? 'Loading…' : 'Connecting to the app…'}</p>
          ) : firstRun ? (
            <FirstRun onDone={() => { setSetupDone(true); setPage('fetching'); }} />
          ) : page === 'build' ? (
            <BuildPage status={status} />
          ) : page === 'champSelect' ? (
            <ChampSelectPage draft={server.draft} status={status} />
          ) : page === 'fetching' ? (
            <FetchingPage status={status} />
          ) : page === 'model' ? (
            <ModelPage />
          ) : page === 'settings' ? (
            <SettingsPage />
          ) : (
            <AboutPage status={status} />
          )}
        </main>
        <footer className="border-t border-line px-5 py-2 text-[11px] text-muted">{LEGAL}</footer>
      </div>
    </DdContext.Provider>
  );
}
