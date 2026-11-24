import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { extractEntry, readEntry, readZip, ZipWriter } from './zip.ts';

const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), 'zip-test-'));

test('a written archive reads back: buffers, a streamed file, UTF-8 names, empty entries', async () => {
  const dir = tmp();
  const big = path.join(dir, 'big.db');
  // Mixed content: half random (stored nearly as is), half repetitive (deflates well); several stream chunks.
  fs.writeFileSync(big, Buffer.concat([crypto.randomBytes(3 << 20), Buffer.alloc(3 << 20, 'irelia')]));
  const out = path.join(dir, 'Agurin-EUW_dataset.zip');
  const w = new ZipWriter(out);
  await w.addBuffer('manifest.json', JSON.stringify({ kind: 'dataset', name: 'Ågurin' }));
  await w.addFile('Agurin-EUW_dataset.db', big);
  await w.addBuffer('empty.txt', '');
  await w.addBuffer('games.csv', 'match,win\nKR_1,1\n'.repeat(1000));
  assert.ok(fs.existsSync(`${out}.partial`) && !fs.existsSync(out), 'written under .partial until finished');
  await w.finish();
  assert.ok(fs.existsSync(out) && !fs.existsSync(`${out}.partial`));

  const entries = await readZip(out);
  assert.deepEqual(entries.map((e) => e.name), ['manifest.json', 'Agurin-EUW_dataset.db', 'empty.txt', 'games.csv']);
  assert.equal(JSON.parse((await readEntry(out, entries[0])).toString('utf8')).name, 'Ågurin');
  const back = path.join(dir, 'back.db');
  await extractEntry(out, entries[1], back);
  assert.ok(fs.readFileSync(back).equals(fs.readFileSync(big)));
  assert.equal((await readEntry(out, entries[2])).length, 0);

  // An independent reader agrees (Python's zipfile checks every CRC with testzip()). Skipped where there is no working Python.
  let py: string | null = null;
  for (const exe of ['python3', 'python']) {
    try {
      py = execFileSync(exe, ['-I', '-c', 'import sys,zipfile;z=zipfile.ZipFile(sys.argv[1]);print(z.testzip());print(len(z.namelist()))', out], { stdio: ['ignore', 'pipe', 'ignore'] }).toString();
      break;
    } catch { /* not installed, or a stub that can't run: try the next */ }
  }
  if (py !== null) assert.equal(py.trim().split(/\r?\n/).join('|'), 'None|4');
});

test('a damaged or incomplete archive says so instead of importing garbage', async () => {
  const dir = tmp();
  const out = path.join(dir, 'a.zip');
  const w = new ZipWriter(out);
  await w.addBuffer('manifest.json', '{"kind":"dataset"}');
  await w.finish();
  const half = path.join(dir, 'half.zip');
  fs.writeFileSync(half, fs.readFileSync(out).subarray(0, 40));
  await assert.rejects(readZip(half), /not a ZIP archive|incomplete/);
  const bytes = fs.readFileSync(out);
  bytes[30 + 'manifest.json'.length + 2] ^= 0xff;   // inside the stored data (after the 30-byte header and the name)
  const bad = path.join(dir, 'bad.zip');
  fs.writeFileSync(bad, bytes);
  const [e] = await readZip(bad);
  await assert.rejects(readEntry(bad, e), /damaged/);
});

test('abort leaves nothing behind', async () => {
  const dir = tmp();
  const out = path.join(dir, 'x.zip');
  const w = new ZipWriter(out);
  await w.addBuffer('a.txt', 'hello');
  await w.abort();
  assert.deepEqual(fs.readdirSync(dir), []);
});
