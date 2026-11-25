// window.buildFetcher: the bridge between the web app and the desktop app, over Tauri's IPC (web/src/desktop.ts types it).
// Only on the app's own pages (its server on 127.0.0.1), and each
// command checks the page's origin again on the Rust side.
(() => {
  if (window.buildFetcher || !/^http:\/\/127\.0\.0\.1:\d+$/.test(location.origin)) return;
  const call = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args || {});
  const on = (name, cb) => {
    const listener = (e) => cb(e.detail);
    window.addEventListener(name, listener);
    return () => { window.removeEventListener(name, listener); };
  };
  const bridge = {
    info: () => call('desktop_info'),
    openFolder: (p) => call('open_folder', { path: String(p) }),
    backup: {
      pickSave: (name) => call('backup_pick_save', { name: String(name) }),
      pickOpen: () => call('backup_pick_open'),
    },
    restart: () => call('restart_app'),
    settings: {
      get: () => call('settings_get'),
      set: (patch) => call('settings_set', { patch: patch ?? {} }),
    },
    overlay: {
      state: () => call('overlay_state'),
      setVisible: (visible) => call('overlay_set_visible', { visible: !!visible }),
      setClickThrough: (on) => call('overlay_set_click_through', { on: !!on }),
      fitHeight: (px) => { call('overlay_fit_height', { px: Number(px) }).catch(() => {}); },
      onState: (cb) => on('bf:overlay-state', cb),
    },
    keepAwake: {
      state: () => call('keep_awake_state'),
      onState: (cb) => on('bf:keep-awake-state', cb),
    },
  };
  Object.defineProperty(window, 'buildFetcher', { value: Object.freeze(bridge), enumerable: true });

  // The overlay while click-through is on: the app sends where the cursor is (it reads its position, no mouse hook),
  // and the tooltip of what is under it shows as hovering would.
  if (location.pathname === '/overlay') {
    let tip = null;
    const hide = () => { if (tip) tip.style.display = 'none'; };
    window.addEventListener('bf:pointer', (e) => {
      const { x, y } = e.detail || {};
      if (typeof x !== 'number' || typeof y !== 'number') { hide(); return; }
      const el = document.elementFromPoint(x, y);
      const titled = el && el.closest ? el.closest('[title]') : null;
      const text = titled ? titled.getAttribute('title') : '';
      if (!text) { hide(); return; }
      if (!tip) {
        tip = document.createElement('div');
        tip.setAttribute('aria-hidden', 'true');
        tip.style.cssText = 'position:fixed;z-index:2147483647;pointer-events:none;max-width:260px;padding:4px 6px;'
          + 'border-radius:4px;background:#111114;color:#e6dcdd;border:1px solid #3a3a40;font:11px/1.35 system-ui;'
          + 'white-space:pre-wrap;box-shadow:0 2px 8px rgba(0,0,0,.5)';
        document.documentElement.appendChild(tip);
      }
      tip.textContent = text;
      tip.style.display = 'block';
      const w = tip.offsetWidth, h = tip.offsetHeight;
      tip.style.left = `${Math.max(2, Math.min(x + 12, window.innerWidth - w - 2))}px`;
      tip.style.top = `${y + 18 + h > window.innerHeight ? Math.max(2, y - h - 6) : y + 18}px`;
    });
  }
})();
