// The app's own server (same origin; Vite proxies /api and /ws in development). Nothing else is ever called, except
// Data Dragon's image CDN for icons (ddragon.ts).
import { useEffect, useRef, useState } from 'react';
import type { DraftState, LcuView, LiveState, ServerEvent, StaticView, Status } from './types.ts';

export class ApiError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: body === undefined ? undefined : { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  const json = text ? JSON.parse(text) : null;
  if (!res.ok) throw new ApiError(json?.error ?? `HTTP ${res.status}`, res.status);
  return json as T;
}

export const api = {
  get: <T>(path: string) => request<T>('GET', path),
  post: <T>(path: string, body?: unknown) => request<T>('POST', path, body ?? {}),
  patch: <T>(path: string, body: unknown) => request<T>('PATCH', path, body),
};

/** One GET, refreshed when `deps` change or every `everyMs`. */
export function useGet<T>(path: string | null, everyMs?: number, deps: unknown[] = []): { data: T | null; error: string | null; reload: () => void } {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!path) return;
    let live = true;
    const load = () =>
      api.get<T>(path).then(
        (d) => { if (live) { setData(d); setError(null); } },
        (e: Error) => { if (live) setError(e.message); },
      );
    load();
    const t = everyMs ? setInterval(load, everyMs) : undefined;
    return () => { live = false; if (t) clearInterval(t); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, everyMs, tick, ...deps]);
  return { data, error, reload: () => setTick((t) => t + 1) };
}

/** Everything /ws pushes, kept current. */
export type ServerState = {
  connected: boolean;
  status: Status | null;
  lcu: LcuView | null;
  draft: DraftState | null;
  live: LiveState | null;
  staticVersion: number;
};

export function useServer(): ServerState {
  const [s, setS] = useState<ServerState>({ connected: false, status: null, lcu: null, draft: null, live: null, staticVersion: 0 });
  const retry = useRef(0);
  useEffect(() => {
    let ws: WebSocket | null = null;
    let closed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const connect = () => {
      ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
      ws.onopen = () => { retry.current = 0; setS((p) => ({ ...p, connected: true })); };
      ws.onmessage = (m) => {
        let ev: ServerEvent;
        try { ev = JSON.parse(String(m.data)); } catch { return; }
        setS((p) => apply(p, ev));
      };
      ws.onclose = () => {
        setS((p) => ({ ...p, connected: false }));
        if (!closed) timer = setTimeout(connect, Math.min(5000, 500 * 2 ** retry.current++));
      };
    };
    connect();
    return () => { closed = true; if (timer) clearTimeout(timer); ws?.close(); };
  }, []);
  return s;
}

export function apply(p: ServerState, ev: ServerEvent): ServerState {
  switch (ev.type) {
    case 'status': return ev.status ? { ...p, status: ev.status, lcu: ev.status.lcu } : p;
    case 'lcu': return { ...p, lcu: ev.state, status: p.status ? { ...p.status, lcu: ev.state } : p.status };
    case 'activity': return { ...p, status: p.status ? { ...p.status, activity: ev.activity } : p.status };
    case 'draft': return { ...p, draft: ev.draft };
    case 'live': return { ...p, live: ev.state.active ? ev.state : null };
    case 'static': return { ...p, staticVersion: p.staticVersion + 1 };
    default: return p;
  }
}

export type StaticData = StaticView;
