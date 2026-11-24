import type { IreliaBridge } from '../../shared/desktop.ts';

declare global {
  interface Window { irelia?: IreliaBridge }
}

/** The Electron preload bridge, or undefined when the dashboard runs in a normal browser (npm run dev). */
export const desktop: IreliaBridge | undefined = window.irelia;
