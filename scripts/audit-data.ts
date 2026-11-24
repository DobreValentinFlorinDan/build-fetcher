// npm run audit:data [-- <data folder>] [--out <file.json>]
// Read-only audit of the stored data (server/audit.ts): no API calls, no writes to the data folder. Prints a compact
// summary and writes the full report as JSON (audit-<date>.json in the current folder unless --out says otherwise).
// Without a folder it uses the installed app's (%APPDATA%\irelia-companion\config.json → dataDir, else
// %APPDATA%\irelia-companion\data), then ./data.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { auditData, formatAudit } from '../server/audit.ts';

const args = process.argv.slice(2);
const outIdx = args.indexOf('--out');
const out = outIdx >= 0 ? args.splice(outIdx, 2)[1] : `audit-${new Date().toISOString().slice(0, 10)}.json`;

function defaultDataDir() {
  if (process.env.IRELIA_DATA_DIR) return process.env.IRELIA_DATA_DIR;
  const appDir = process.env.IRELIA_CONFIG_DIR ?? path.join(process.env.APPDATA ?? path.join(os.homedir(), '.config'), 'irelia-companion');
  try {
    const dataDir = (JSON.parse(fs.readFileSync(path.join(appDir, 'config.json'), 'utf8')) as { dataDir?: string }).dataDir;
    if (dataDir && fs.existsSync(path.join(dataDir, 'irelia.db'))) return dataDir;
  } catch { /* no installed app settings */ }
  if (fs.existsSync(path.join(appDir, 'data', 'irelia.db'))) return path.join(appDir, 'data');
  return path.resolve('data');
}

const dataDir = path.resolve(args[0] ?? defaultDataDir());
console.error(`Auditing ${dataDir} (read-only, no API calls)…`);
const report = await auditData(dataDir, {
  onProgress: (p) => process.stderr.write(`\r  ${p.label}${p.part !== undefined ? ` ${Math.round(p.part * 100)}%` : ''}…            `),
});
process.stderr.write('\n');
console.log(formatAudit(report));
fs.writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
console.error(`\nFull report: ${path.resolve(out)}`);
