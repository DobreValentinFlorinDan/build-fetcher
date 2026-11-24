import fs from 'node:fs';
import path from 'node:path';
import { appConfigDir } from '../appConfig.ts';

/**
 * Use GPU's log (0.13.0): %APPDATA%/irelia-companion/gpu.log, shown in Settings → GPU. Every thread that opens the GPU
 * writes what it found, the checks and what it did; nothing else goes in it. Kept under about half a megabyte: past
 * that, its older half goes.
 */
export const gpuLogFile = path.join(appConfigDir, 'gpu.log');
const MAX_BYTES = 512 * 1024;

export function gpuLog(line: string) {
  try {
    fs.mkdirSync(appConfigDir, { recursive: true });
    fs.appendFileSync(gpuLogFile, `${new Date().toISOString()}  ${line}\n`);
    if (fs.statSync(gpuLogFile).size > MAX_BYTES) {
      const text = fs.readFileSync(gpuLogFile, 'utf8');
      fs.writeFileSync(gpuLogFile, text.slice(text.indexOf('\n', text.length - MAX_BYTES / 2) + 1));
    }
  } catch { /* a log that can't be written loses the line, nothing else */ }
}

/** The log's last `lines` lines ('' before anything was written). */
export function readGpuLog(lines = 400): string {
  try { return fs.readFileSync(gpuLogFile, 'utf8').split('\n').slice(-lines - 1).join('\n').trimEnd(); } catch { return ''; }
}

export function clearGpuLog() {
  try { fs.rmSync(gpuLogFile, { force: true }); } catch { /* nothing to clear */ }
}
