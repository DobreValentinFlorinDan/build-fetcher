import { useState } from 'react';
import type { BackupStatus, TransferProgress } from '../../../shared/types.ts';
import { api, fmtBytes, type ImportPreview } from '../api.ts';
import { desktop } from '../desktop.ts';
import { Card } from './common.tsx';

// Backup of the pool (0.18.0, server/autoExport.ts): the backup folder (a Google Drive folder synced to this computer),
// Back up now, and importing a ZIP made on another computer.

const BUTTON = 'rounded-md border border-ink-600 px-2.5 py-1 text-xs font-semibold text-ash-200 hover:border-blood-500 disabled:opacity-40';
const ago = (t: number | null) => {
  if (!t) return 'never';
  const m = Math.round((Date.now() - t) / 60_000);
  return m < 1 ? 'just now' : m < 60 ? `${m} min ago` : m < 48 * 60 ? `${Math.round(m / 60)} h ago` : `${Math.round(m / 1440)} days ago`;
};
const date = (t: number) => new Date(t).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** A folder: the system's picker in the app; a typed path in a browser (npm run dev). */
async function pickFolder(title: string): Promise<string | null> {
  if (desktop) return desktop.selectDirectory(undefined, title);
  const p = window.prompt(`${title}\n\nFull path of the folder:`);
  return p?.trim() || null;
}
async function pickZip(): Promise<string | null> {
  if (desktop) return desktop.selectZip();
  const p = window.prompt('Full path of the ZIP to import:');
  return p?.trim() || null;
}

function Transfer({ t }: { t: TransferProgress | null }) {
  if (!t) return null;
  const running = t.finishedAt === null;
  if (!running && Date.now() - t.finishedAt! > 10 * 60_000) return null;
  return (
    <div className={`mt-2 rounded-md border px-2 py-1.5 text-xs ${t.error ? 'border-blood-700 text-blood-200' : 'border-ink-600 text-ash-200'}`} role="status">
      <div className="flex items-center justify-between gap-2">
        <span className="min-w-0 truncate">{running ? t.label : t.error ? `${t.label}: ${t.error}` : t.result}</span>
        {running && t.fraction !== null && <span className="tabular-nums">{Math.round(t.fraction * 100)}%</span>}
        {!running && t.file && desktop && (
          <button type="button" className={BUTTON} onClick={() => void desktop!.openFolder(t.file!.replace(/[\\/][^\\/]*$/, ''))}>Show folder</button>
        )}
      </div>
      {running && <div className="mt-1 h-1 overflow-hidden rounded bg-ink-800"><div className="h-full bg-blood-500 transition-[width]" style={{ width: `${Math.round((t.fraction ?? 0) * 100)}%` }} /></div>}
    </div>
  );
}

const IMPORT_WARNING = "This adds the games this computer doesn't have to its pool: the pool's builds will include them, and the automatic evaluation runs again. Nothing is replaced.";

/** Data tab: back up the pool to a folder, and bring in a backup from another computer. */
export function BackupCard({ status }: { status: BackupStatus | null }) {
  const [error, setError] = useState<string | null>(null);
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const busy = !!status?.transfer && status.transfer.finishedAt === null;
  const auto = status?.autoExport ?? null;
  const act = (work: () => Promise<unknown>) => { setError(null); work().catch((e) => setError(errorText(e))); };
  const chooseFolder = async (title: string) => {
    const folder = await pickFolder(title);
    if (folder) act(() => api.setBackupFolder(folder));
  };
  const importFile = async () => {
    const file = await pickZip();
    if (!file) return;
    setError(null);
    api.inspectImport(file).then(setPreview, (e) => setError(errorText(e)));
  };
  return (
    <Card title="Backup" aside={auto ? <span className="text-[11px] text-ash-600">last {ago(auto.last)}</span> : undefined}>
      <p className="mb-2 text-[11px] leading-snug text-ash-600">
        Saves the high-elo pool as a ZIP in a folder you choose, for example your Google Drive folder, once a week (each computer keeps
        its own 3 newest). Other players' IDs are left out. Point another computer's app at the same folder and it offers the
        newer backup for import: it never imports by itself, and an import only adds the games this computer doesn't have. A
        backup also carries its computer's last evaluation: importing it spares this one evaluating those games again.
      </p>
      {auto ? <>
        <p className="break-all text-[11px] text-ash-600">Backing up to <code>{auto.folder}</code></p>
        {auto.error && <p className="mt-1 text-xs text-blood-300">The last backup failed: {auto.error}</p>}
        <div className="mt-1.5 flex flex-wrap gap-1.5">
          <button type="button" className={BUTTON} disabled={busy} onClick={() => act(() => api.backUpNow())}>Back up now</button>
          <button type="button" className={BUTTON} onClick={() => void chooseFolder('Backup folder (for example in Google Drive)')}>Change folder</button>
          <button type="button" className={BUTTON} onClick={() => act(() => api.setBackupFolder(null))}>Turn off</button>
        </div>
      </> : (
        <button type="button" className={BUTTON} onClick={() => void chooseFolder('Backup folder (for example in Google Drive)')}>Choose a backup folder…</button>
      )}
      {error && <p className="mt-2 text-xs text-blood-300">{error}</p>}
      <Transfer t={status?.transfer ?? null} />

      {status?.offers.map((o) => (
        <div key={o.file} className="mt-2 rounded-md border border-gold-900 bg-gold-900/20 px-2 py-1.5 text-xs text-ash-200" role="status">
          A newer backup from another computer: {o.games.toLocaleString('en-US')} games, {date(o.exportedAt)} ({fmtBytes(o.bytes)}).
          <span className="mt-1 flex gap-1.5">
            <button type="button" className={BUTTON} disabled={busy} onClick={() => {
              if (window.confirm(`Import ${o.games.toLocaleString('en-US')} games into this computer's pool?\n\n${IMPORT_WARNING}`)) act(() => api.importZip(o.file));
            }}>Import</button>
            <button type="button" className={BUTTON} onClick={() => act(() => api.dismissImport(o.file, o.exportedAt))}>Not now</button>
          </span>
        </div>
      ))}

      {preview && (
        <div className="mt-2 rounded-md border border-ink-600 px-2 py-1.5 text-xs text-ash-200">
          Backup of the pool: {preview.games.toLocaleString('en-US')} games ({preview.patches.map((p) => p.patch).join(', ')}),
          made {date(preview.exportedAt)} on {preview.machine} · {fmtBytes(preview.bytes)}
          <p className="mt-1 text-gold-400">{IMPORT_WARNING}</p>
          <span className="mt-1 flex gap-1.5">
            <button type="button" className={BUTTON} disabled={busy} onClick={() => { const p = preview; setPreview(null); act(() => api.importZip(p.file)); }}>Import into the pool</button>
            <button type="button" className={BUTTON} onClick={() => setPreview(null)}>Cancel</button>
          </span>
        </div>
      )}
      <div className="mt-2 border-t border-ink-600 pt-2">
        <button type="button" className={BUTTON} disabled={busy} onClick={() => void importFile()}>Import a backup ZIP…</button>
      </div>
    </Card>
  );
}
