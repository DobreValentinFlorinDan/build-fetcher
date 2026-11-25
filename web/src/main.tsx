import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App, useStatic } from './App.tsx';
import { useServer } from './api.ts';
import { DdContext } from './ddragon.ts';
import { Overlay } from './Overlay.tsx';
import './styles.css';

function OverlayRoot() {
  const server = useServer();
  return <DdContext.Provider value={useStatic(server)}><Overlay /></DdContext.Provider>;
}

// /overlay is the in-game HUD (a transparent window in the desktop app); everything else is the dashboard.
const overlay = location.pathname.replace(/\/+$/, '') === '/overlay';
createRoot(document.getElementById('root')!).render(<StrictMode>{overlay ? <OverlayRoot /> : <App />}</StrictMode>);
