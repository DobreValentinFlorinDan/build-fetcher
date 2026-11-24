import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App.tsx';
import { OverlayView } from './components/OverlayView.tsx';
import './styles.css';

// /overlay is the in-game HUD (transparent window in the desktop app); everything else is the dashboard.
const overlay = location.pathname.replace(/\/+$/, '') === '/overlay';
createRoot(document.getElementById('root')!).render(<StrictMode>{overlay ? <OverlayView /> : <App />}</StrictMode>);
