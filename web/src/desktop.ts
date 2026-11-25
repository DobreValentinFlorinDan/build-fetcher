// window.buildFetcher: the desktop app's bridge (desktop/tauri/src/bridge.js). Undefined in a plain browser (npm run dev).

export type OverlayState = { visible: boolean; clickThrough: boolean; inGame: boolean; hotkeyErrors: string[] };
export type DesktopInfo = { version: string; packaged: boolean; userData: string; logFile: string };
export type DesktopSettings = {
  overlay: { autoShow: boolean; clickThrough: boolean; opacity: number };
  hotkeys: { toggleOverlay: string; toggleClickThrough: string };
  closeToTray: boolean;
  keepAwakeWhileFetching: boolean;
  keepScreenOnWhileFetching: boolean;
};

export type Bridge = {
  info(): Promise<DesktopInfo>;
  openFolder(path: string): Promise<void>;
  /** Native file dialogs for Settings → Storage; null when cancelled. */
  backup: { pickSave(name: string): Promise<string | null>; pickOpen(): Promise<string | null> };
  restart(): Promise<void>;
  settings: { get(): Promise<DesktopSettings>; set(patch: unknown): Promise<DesktopSettings> };
  overlay: {
    state(): Promise<OverlayState>;
    setVisible(v: boolean): Promise<OverlayState>;
    setClickThrough(on: boolean): Promise<OverlayState>;
    fitHeight(px: number): void;
    onState(cb: (s: OverlayState) => void): () => void;
  };
  keepAwake: { state(): Promise<unknown>; onState(cb: (s: unknown) => void): () => void };
};

declare global {
  interface Window { buildFetcher?: Bridge }
}

export const desktop: Bridge | undefined = typeof window === 'undefined' ? undefined : window.buildFetcher;
