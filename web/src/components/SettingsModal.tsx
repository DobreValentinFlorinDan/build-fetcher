import { useEffect, useRef, useState } from 'react';
import type { DesktopInfo, DesktopSettings, KeepAwakeState, OverlayState } from '../../../shared/desktop.ts';
import type { GpuState, ScraperStatus, SettingsState, StorageAction, StorageProgress } from '../../../shared/types.ts';
import { api } from '../api.ts';
import { desktop } from '../desktop.ts';
import { DetailsToggle, SwitchRow, useShowDetails } from './common.tsx';
import { StorageManager } from './StorageManager.tsx';

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="rounded-lg border border-ember bg-ink-800 p-4">
      <h3 className="mb-3 text-xs font-semibold uppercase tracking-[0.14em] text-ash-400">{title}</h3>
      {children}
    </section>
  );
}


// ------------------------------------------------------------------ Riot API key

/** "saved 3 h ago": development keys stop working 24 h after they are generated, so the age is worth seeing. */
function keyAge(savedAt: number | null) {
  if (!savedAt) return null;
  const h = (Date.now() - savedAt) / 3600_000;
  const ago = h < 1 ? `${Math.max(1, Math.round(h * 60))} min ago` : h < 48 ? `${Math.round(h)} h ago` : `${Math.round(h / 24)} days ago`;
  return { ago, stale: h >= 24, when: new Date(savedAt).toLocaleString() };
}

function ApiKey() {
  const [state, setState] = useState<SettingsState | null>(null);
  const [key, setKey] = useState('');
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  useEffect(() => { api.settings().then(setState).catch(() => {}); }, []);

  const save = (value: string) => api.saveApiKey(value)
    .then((s) => { setState(s); setKey(''); setMsg({ ok: true, text: value ? 'Key saved. The scraper uses it from the next request.' : 'Saved key removed.' }); })
    .catch((e) => setMsg({ ok: false, text: e instanceof Error ? e.message : String(e) }));

  return (
    <div className="space-y-2 text-sm">
      <p className="text-ash-400">
        {state?.riotApiKey ? <>Current key <code className="text-ash-200">{state.riotApiKey}</code> from {state.keySource === 'settings' ? 'these settings' : '.env'}.</> : 'No key configured: fetching is disabled.'}
        {' '}Development keys expire 24 h after you generate them; personal and production keys don't.{' '}
        <a href="https://developer.riotgames.com" target="_blank" rel="noreferrer" className="text-blood-300 underline">Get a key</a>
      </p>
      <form className="flex gap-2" onSubmit={(e) => { e.preventDefault(); void save(key); }}>
        <input type="password" value={key} onChange={(e) => setKey(e.target.value)} placeholder="RGAPI-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
          autoComplete="off" spellCheck={false} aria-label="Riot API key"
          className="min-w-0 flex-1 rounded-md border border-ink-600 bg-ink-950 px-2 py-1.5 font-mono text-xs text-ash-50 outline-none focus:border-blood-500" />
        <button type="submit" disabled={!key.trim()} className="rounded-md bg-blood-600 px-3 py-1.5 text-xs font-bold text-ash-50 hover:bg-blood-500 disabled:opacity-40">Save</button>
        {state?.keySource === 'settings' && (
          <button type="button" onClick={() => void save('')} className="rounded-md border border-ink-600 px-3 py-1.5 text-xs text-ash-200 hover:border-blood-500">Remove</button>
        )}
      </form>
      {(() => {
        const age = keyAge(state?.keySavedAt ?? null);
        return age && (
          <p className={`text-xs ${age.stale ? 'text-gold-400' : 'text-ash-600'}`} title={age.when}>
            Saved {age.ago}.{age.stale && ' A development key is expired by now: paste a new one (or use a personal key, which lasts).'}
          </p>
        );
      })()}
      {msg && <p className={`text-xs ${msg.ok ? 'text-[#2dbf8d]' : 'text-blood-300'}`}>{msg.text}</p>}
    </div>
  );
}

// ------------------------------------------------------------------ your account

/**
 * Whose games "Your games" shows: the account signed in to the League client, found through the Riot API by its Riot
 * ID. The server is detected; when the client and Riot can't tell, it is chosen here (and a choice here wins).
 */
function Account({ scraper }: { scraper: ScraperStatus | null }) {
  const [state, setState] = useState<SettingsState | null>(null);
  useEffect(() => { api.settings().then(setState).catch(() => {}); }, []);
  const me = scraper?.me ?? null;
  const problem = scraper?.lastError && /account|Riot ID|which server|API key in Settings/.test(scraper.lastError) ? scraper.lastError : null;
  const choose = (v: string) => { void api.setMyPlatform(v || null).then(setState).catch(() => {}); };
  return (
    <div className="space-y-2 text-sm">
      <p className="text-ash-400">
        {me ? <>Fetching the Irelia games of <b className="text-ash-50">{me.riotId ?? 'your account'}</b> on {me.platform}: {me.games} stored.</>
          : 'Open the League client with the app running: your account is read from it, and your ranked Irelia games are fetched after each game.'}
      </p>
      {problem && <p className="text-xs text-gold-400">{problem}</p>}
      <label className="flex items-center gap-2 text-ash-200">
        <span>Your server</span>
        <select value={state?.myPlatform ?? ''} onChange={(e) => choose(e.target.value)} aria-label="Your server"
          className="rounded-md border border-ink-600 bg-ink-950 px-2 py-1 text-xs text-ash-50 outline-none focus:border-blood-500">
          <option value="">Detect automatically</option>
          {state?.platforms.map((p) => <option key={p.id} value={p.id}>{p.label} ({p.id})</option>)}
        </select>
      </label>
    </div>
  );
}

// ------------------------------------------------------------------ while you play

/** Champ select and games (0.14.0): fetching pauses; the middleware also runs at lower priority then. */
function WhilePlaying({ scraper }: { scraper: ScraperStatus | null }) {
  const [state, setState] = useState<SettingsState | null>(null);
  useEffect(() => { api.settings().then(setState).catch(() => {}); }, []);
  if (!state) return null;
  const toggle = (v: boolean) => { setState({ ...state, pauseFetchingInGame: v }); void api.setPauseFetchingInGame(v).then(setState).catch(() => {}); };
  return (
    <div className="text-sm">
      <SwitchRow label="Pause fetching during games" checked={state.pauseFetchingInGame} onChange={toggle}
        hint={<>
          In champ select and in game, no Riot API requests and no database writes, so League has the network, the disk and the processor. Fetching resumes when the game ends, and your new game is fetched then.
          {scraper?.paused === 'game' && <span className="mt-0.5 block text-ash-400">Paused now: fetching resumes when the game ends.</span>}
        </>} />
      <p className="mt-1 text-[11px] text-ash-600">During champ select and games the app also runs at below-normal priority, and frees the memory it doesn't need when champ select starts.</p>
    </div>
  );
}

// ------------------------------------------------------------------ GPU

const BUTTON = 'rounded-md border border-ink-600 px-3 py-1.5 text-xs font-semibold text-ash-200 hover:border-blood-500 disabled:opacity-40';

/**
 * Use GPU (0.13.0, server/gpu/): Evaluate builds' draft similarities on an NVIDIA graphics card, with the same numbers;
 * Test GPU, and the log of what it found and did, to switch back by when something goes wrong.
 */
function GpuSection() {
  const [state, setState] = useState<GpuState | null>(null);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [copied, setCopied] = useState(false);
  const logRef = useRef<HTMLPreElement>(null);
  const [details] = useShowDetails();
  useEffect(() => {
    const load = () => api.gpu().then(setState).catch(() => {});
    void load();
    // An evaluation writes to the log as it goes.
    const timer = setInterval(load, 5000);
    return () => clearInterval(timer);
  }, []);
  // The newest lines in view.
  useEffect(() => { const el = logRef.current; if (el) el.scrollTop = el.scrollHeight; }, [state?.log, details]);
  if (!state) return null;
  const fail = (e: unknown) => setMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
  const toggle = (on: boolean) => api.setUseGpu(on).then((s) => { setState(s); setMsg(null); }).catch(fail);
  const test = () => {
    setState({ ...state, testing: true });
    setMsg(null);
    api.testGpu()
      .then((s) => {
        setState(s);
        setMsg(s.ok ? { ok: true, text: 'The GPU passed: the same numbers as the processor. How much faster it is, is in the log.' }
          : { ok: false, text: "The GPU can't be used here: the log says why." });
      })
      .catch((e) => { fail(e); void api.gpu().then(setState).catch(() => {}); });
  };
  const copy = () => navigator.clipboard.writeText(state.log)
    .then(() => { setCopied(true); setTimeout(() => setCopied(false), 1500); })
    .catch(fail);

  return (
    <div className="space-y-2 text-sm">
      <SwitchRow label="Use GPU" checked={state.useGpu} onChange={(v) => void toggle(v)}
        hint={<>
          Evaluate builds works out how alike each draft is to every stored game on your NVIDIA graphics card (CUDA), the
          larger part of its work, instead of on the processor alone. The report stays the same to the last digit: the GPU's
          numbers are checked against the processor's before each evaluation and now and then while it runs. If they ever
          differ, or the GPU fails, the evaluation goes on on the processor by itself, and the log below says why. Not used
          in champ select or a game. Switch it off to go back to the processor.
        </>} />
      <div className="flex flex-wrap items-center gap-2">
        <button type="button" disabled={state.testing} onClick={test} className={BUTTON}>{state.testing ? 'Testing…' : 'Test GPU'}</button>
        <button type="button" disabled={!state.log} onClick={() => void copy()} className={BUTTON}>{copied ? 'Copied' : 'Copy log'}</button>
        <button type="button" disabled={!state.log} onClick={() => void api.clearGpuLog().then(setState).catch(fail)} className={BUTTON}>Clear log</button>
      </div>
      {msg && <p className={`text-xs ${msg.ok ? 'text-[#2dbf8d]' : 'text-gold-400'}`}>{msg.text}</p>}
      {/* Hidden with Hide details (0.18.0): the log is still written, and Copy log still copies it. */}
      {details ? (
        <pre ref={logRef} aria-label="GPU log"
          className="max-h-56 overflow-auto whitespace-pre-wrap break-words rounded-md border border-ink-600 bg-ink-950 p-2 font-mono text-[11px] leading-snug text-ash-400">
          {state.log || 'Nothing logged yet: Test GPU, or run Evaluate builds with Use GPU on.'}
        </pre>
      ) : <p className="text-[11px] text-ash-600">The GPU log is hidden (it is still written).</p>}
      <div className="flex justify-end"><DetailsToggle /></div>
      <p className="text-[11px] text-ash-600">Saved in <code className="break-all">{state.logFile}</code>.</p>
    </div>
  );
}

// ------------------------------------------------------------------ desktop + overlay

const MODIFIERS = new Set(['Control', 'Shift', 'Alt', 'Meta']);
const KEY_NAMES: Record<string, string> = { ' ': 'Space', ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right', '+': 'Plus' };

/** KeyboardEvent → Electron accelerator ("Ctrl+Shift+Tab"). Null until a non-modifier key is pressed. */
function toAccelerator(e: React.KeyboardEvent): string | null {
  if (MODIFIERS.has(e.key)) return null;
  const key = KEY_NAMES[e.key] ?? (e.key.length === 1 ? e.key.toUpperCase() : e.key);
  return [e.ctrlKey && 'Ctrl', e.altKey && 'Alt', e.shiftKey && 'Shift', e.metaKey && 'Super', key].filter(Boolean).join('+');
}

function HotkeyInput({ label, value, onChange }: { label: string; value: string; onChange: (v: string) => void }) {
  const [recording, setRecording] = useState(false);
  const [hint, setHint] = useState<string | null>(null);
  return (
    <label className="block text-sm text-ash-200">
      {label}
      <input readOnly value={recording ? 'Press keys…' : value}
        onFocus={() => setRecording(true)} onBlur={() => { setRecording(false); setHint(null); }}
        onKeyDown={(e) => {
          e.preventDefault();
          if (e.key === 'Escape') { (e.target as HTMLInputElement).blur(); return; }
          const acc = toAccelerator(e);
          if (!acc) return;
          // A bare key would be stolen from the game while it runs (e.g. "Q"): require a modifier or an F-key.
          if (!/^(Ctrl|Alt|Shift|Super)\+/.test(acc) && !/^F\d{1,2}$/.test(acc)) { setHint('Use Ctrl, Alt or Shift (or an F-key) so the game keeps the key.'); return; }
          if (acc === 'Alt+Tab') { setHint('Alt+Tab belongs to Windows and cannot be registered.'); return; }
          onChange(acc); setHint(null); (e.target as HTMLInputElement).blur();
        }}
        className={`mt-1 block w-full rounded-md border bg-ink-950 px-2 py-1.5 font-mono text-xs text-ash-50 outline-none ${recording ? 'border-blood-500' : 'border-ink-600'}`} />
      {hint && <span className="text-[11px] text-gold-400">{hint}</span>}
    </label>
  );
}

/** Whether the computer is being kept awake right now, in a few words (desktop/keepAwake.ts). */
function keepAwakeNow(k: KeepAwakeState | null, enabled: boolean): string | null {
  if (!enabled || !k) return null;
  if (!k.fetching && !k.evaluating) return 'Not active now: fetching is stopped and no evaluation runs.';
  if (k.system === 'failed') {
    return k.screen ? `Keeping the screen on (and so the computer awake); the screen-off request failed: ${k.error ?? 'unknown error'}.`
      : `Couldn't keep the computer awake (${k.error ?? 'unknown error'}), so it may still sleep. Keep the screen on too always works.`;
  }
  if (k.system === 'starting') return 'Starting…';
  const what = k.fetching ? 'fetching runs' : 'the evaluation runs';
  return k.screen ? `Active now: the computer stays awake and the screen on while ${what}.` : `Active now: the computer stays awake while ${what}.`;
}

function DesktopSection() {
  const [s, setS] = useState<DesktopSettings | null>(null);
  const [ov, setOv] = useState<OverlayState | null>(null);
  const [info, setInfo] = useState<DesktopInfo | null>(null);
  const [awake, setAwake] = useState<KeepAwakeState | null>(null);
  useEffect(() => {
    if (!desktop) return;
    desktop.settings.get().then(setS).catch(() => {});
    desktop.overlay.state().then(setOv).catch(() => {});
    desktop.info().then(setInfo).catch(() => {});
    const offOverlay = desktop.overlay.onState(setOv);
    // The keep-awake status (an older desktop shell has no keepAwake: the status line is left out).
    const k = desktop.keepAwake;
    k?.state().then(setAwake).catch(() => {});
    const offAwake = k?.onState(setAwake);
    return () => { offOverlay(); offAwake?.(); };
  }, []);
  if (!desktop || !s) return null;
  const bridge = desktop;
  const set = (patch: Parameters<typeof bridge.settings.set>[0]) => bridge.settings.set(patch).then(setS);

  return (
    <Section title="Desktop & in-game overlay">
      <div className="grid gap-x-6 gap-y-1 sm:grid-cols-2">
        <div>
          <SwitchRow label="Show the overlay when a game starts" checked={s.overlay.autoShow} onChange={(v) => void set({ overlay: { autoShow: v } })}
            hint="Run League in Borderless or Windowed mode; exclusive Fullscreen draws over every other window." />
          <SwitchRow label="Click-through by default" checked={s.overlay.clickThrough} onChange={(v) => void set({ overlay: { clickThrough: v } })}
            hint="Mouse input goes straight to the game. Unlock with the hotkey to move the HUD." />
          <SwitchRow label="Keep running in the tray when the window closes" checked={s.closeToTray} onChange={(v) => void set({ closeToTray: v })} />
          <SwitchRow label="Keep the computer awake while fetching or evaluating" checked={s.keepAwakeWhileFetching} onChange={(v) => void set({ keepAwakeWhileFetching: v })}
            hint={<>
              Windows doesn't go to sleep while fetching runs or an evaluation you started (Evaluate builds); the screen can still turn off. It ends when both stop, or when a development API key expires (24 h after you generate it). Putting the computer to sleep yourself still works.
              {keepAwakeNow(awake, s.keepAwakeWhileFetching) && <span className={`mt-0.5 block ${awake?.system === 'failed' ? 'text-gold-400' : 'text-ash-400'}`}>{keepAwakeNow(awake, s.keepAwakeWhileFetching)}</span>}
            </>} />
          {s.keepAwakeWhileFetching && (
            <SwitchRow label="Keep the screen on too" checked={s.keepScreenOnWhileFetching} onChange={(v) => void set({ keepScreenOnWhileFetching: v })}
              hint="Only if the computer still sleeps while fetching or evaluating: Windows always honours this one, but the screen stays on." />
          )}
          <label className="mt-2 block text-sm text-ash-200">
            Overlay opacity <b className="tabular-nums">{Math.round(s.overlay.opacity * 100)}%</b>
            <input type="range" min={40} max={100} value={Math.round(s.overlay.opacity * 100)}
              onChange={(e) => void set({ overlay: { opacity: Number(e.target.value) / 100 } })} className="mt-1 block w-full accent-blood-500" />
          </label>
        </div>
        <div className="space-y-2">
          <HotkeyInput label="Show / hide overlay" value={s.hotkeys.toggleOverlay} onChange={(v) => void set({ hotkeys: { toggleOverlay: v } })} />
          <HotkeyInput label="Click-through on / off" value={s.hotkeys.toggleClickThrough} onChange={(v) => void set({ hotkeys: { toggleClickThrough: v } })} />
          <p className="text-[11px] text-ash-600">Hotkeys are registered only while a game is running, so they never take keys away from other apps.</p>
          {!!ov?.hotkeyErrors.length && <p className="text-xs text-blood-300">Windows refused: {ov.hotkeyErrors.join(', ')} (taken by another app).</p>}
        </div>
      </div>
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <button type="button" onClick={() => void bridge.overlay.setVisible(!ov?.visible)}
          className="rounded-md border border-ink-600 px-3 py-1.5 text-xs font-semibold text-ash-200 hover:border-blood-500">{ov?.visible ? 'Hide overlay' : 'Show overlay now'}</button>
        <button type="button" onClick={() => { void bridge.overlay.setVisible(true); void bridge.overlay.setClickThrough(false); }}
          className="rounded-md border border-ink-600 px-3 py-1.5 text-xs font-semibold text-ash-200 hover:border-blood-500">Unlock to reposition</button>
        {info && (
          <span className="ml-auto text-[11px] text-ash-600">
            v{info.version} · <button type="button" className="underline hover:text-ash-400" onClick={() => void bridge.openFolder(info.userData)}>open app folder</button>
          </span>
        )}
      </div>
    </Section>
  );
}

// ------------------------------------------------------------------ modal

export function SettingsModal({ open, onClose, storageProgress, storageAction, scraper }: {
  open: boolean; onClose: () => void; storageProgress: StorageProgress | null; storageAction: StorageAction | null; scraper: ScraperStatus | null;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);

  return (
    <dialog ref={ref} onClose={onClose} aria-labelledby="settings-title"
      className="m-auto max-h-[90vh] w-[min(760px,calc(100vw-32px))] overflow-y-auto rounded-xl border border-ember bg-ink-900 p-0 text-ash-200 backdrop:bg-black/70">
      <header className="sticky top-0 z-10 flex items-center justify-between border-b border-ember bg-ink-900 px-5 py-3">
        <h2 id="settings-title" className="text-base font-bold text-ash-50">Settings</h2>
        <button type="button" onClick={onClose} aria-label="Close settings" className="rounded px-2 text-lg text-ash-400 hover:text-ash-50">✕</button>
      </header>
      {open && (
        <div className="space-y-3 p-5">
          <Section title="Storage"><StorageManager progress={storageProgress} running={storageAction} /></Section>
          <Section title="Riot API key"><ApiKey /></Section>
          <Section title="Your account"><Account scraper={scraper} /></Section>
          <Section title="While you play"><WhilePlaying scraper={scraper} /></Section>
          <Section title="GPU"><GpuSection /></Section>
          <DesktopSection />
        </div>
      )}
    </dialog>
  );
}
