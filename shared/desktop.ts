// Contract between the Electron shell (desktop/), its preload bridge and the React app.
// Lives in shared/ so all three type-check against the same shapes. Types only: no runtime imports.

export interface OverlayBounds { x: number; y: number; width: number }

export interface DesktopSettings {
  overlay: {
    autoShow: boolean;       // show the HUD when a game starts, hide it when the game ends
    clickThrough: boolean;   // start every game click-through (mouse goes to the game)
    opacity: number;         // 0.4 .. 1
    bounds?: OverlayBounds;  // last position; unset = right edge of the primary display
  };
  /** Electron accelerators. Registered only while a game is running, so they never shadow other apps. */
  hotkeys: { toggleOverlay: string; toggleClickThrough: string };
  closeToTray: boolean;
  /** Windows doesn't go to sleep while fetching runs, or an evaluation you started (0.19.0; the screen can still turn off). */
  keepAwakeWhileFetching: boolean;
  /** ...and the screen stays on too: Windows always honours this one (desktop/keepAwake.ts). */
  keepScreenOnWhileFetching: boolean;
}

export const DEFAULT_DESKTOP_SETTINGS: DesktopSettings = {
  overlay: { autoShow: true, clickThrough: true, opacity: 0.92 },
  hotkeys: { toggleOverlay: 'Shift+Tab', toggleClickThrough: 'Ctrl+Shift+Tab' },
  closeToTray: true,
  keepAwakeWhileFetching: true,
  keepScreenOnWhileFetching: false,
};

/**
 * Keeping the computer awake right now (desktop/keepAwake.ts): `system` is the request that stops idle sleep (on
 * Windows a helper holding ES_SYSTEM_REQUIRED: 'starting' until it holds, 'failed' with `error` when it couldn't),
 * `screen` whether the display is kept on too, `fetching` whether fetching runs and `evaluating` whether an evaluation
 * you started does (0.19.0): it is only wanted then.
 */
export interface KeepAwakeState {
  active: boolean;
  system: 'off' | 'starting' | 'held' | 'failed';
  screen: boolean;
  error: string | null;
  fetching?: boolean;
  evaluating?: boolean;
}

export interface OverlayState {
  visible: boolean;
  clickThrough: boolean;
  inGame: boolean;
  hotkeyErrors: string[];  // accelerators Windows refused (already taken by another app)
}

export interface DesktopInfo { version: string; packaged: boolean; userData: string; logFile: string }

/** `window.irelia`, exposed by desktop/preload.ts. Absent when the dashboard runs in a normal browser. */
export interface IreliaBridge {
  info(): Promise<DesktopInfo>;
  /** A folder picker; `title` names what it is for (default: the data folder). */
  selectDirectory(defaultPath?: string, title?: string): Promise<string | null>;
  /** A ZIP file picker (0.18.0: importing an export). */
  selectZip(title?: string): Promise<string | null>;
  openFolder(path: string): Promise<void>;
  settings: {
    get(): Promise<DesktopSettings>;
    set(patch: {
      overlay?: Partial<DesktopSettings['overlay']>; hotkeys?: Partial<DesktopSettings['hotkeys']>; closeToTray?: boolean;
      keepAwakeWhileFetching?: boolean; keepScreenOnWhileFetching?: boolean;
    }): Promise<DesktopSettings>;
  };
  /** Whether the computer is being kept awake right now, and changes to it. */
  keepAwake: {
    state(): Promise<KeepAwakeState>;
    onState(cb: (s: KeepAwakeState) => void): () => void;
  };
  overlay: {
    state(): Promise<OverlayState>;
    setVisible(visible: boolean): Promise<OverlayState>;
    setClickThrough(on: boolean): Promise<OverlayState>;
    /** Overlay renderer reports its content height so the transparent window hugs the HUD. */
    fitHeight(px: number): void;
    onState(cb: (s: OverlayState) => void): () => void;
  };
}
