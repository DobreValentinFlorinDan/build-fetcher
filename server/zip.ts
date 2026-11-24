// ZIP files for exporting and importing data (0.18.0): a streaming writer and a reader, on Node's own zlib, so a
// multi-gigabyte pool export never sits in memory and no dependency is added. ZIP64 is used where a size or an offset
// needs it (a pool's database can be past 4 GB); Windows Explorer, macOS and Google Drive's preview all read it.
//
// The writer: `new ZipWriter(file)`, then `addFile(name, path)` / `addBuffer(name, data)` one after another, then
// `finish()`. It writes to `<file>.partial` and renames it at the end, so a folder that syncs (Google Drive) never
// uploads a half-written archive. The reader: `readZip(file)` lists the entries; `extractEntry` and `readEntry` take
// one out, checking its CRC-32.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import { pipeline } from 'node:stream/promises';
import { Transform } from 'node:stream';
import zlib from 'node:zlib';

const SIG_LOCAL = 0x04034b50;
const SIG_CENTRAL = 0x02014b50;
const SIG_DESCRIPTOR = 0x08074b50;
const SIG_EOCD = 0x06054b50;
const SIG_EOCD64 = 0x06064b50;
const SIG_LOCATOR64 = 0x07064b50;
const MAX32 = 0xffffffff;
const MAX16 = 0xffff;
const FLAG_DESCRIPTOR = 1 << 3;
const FLAG_UTF8 = 1 << 11;
const DEFLATE = 8;
const STORE = 0;

/** CRC-32 as ZIP wants it (zlib.crc32: Node 20.15 / 22.2, so every Electron this app ships with). */
const crc32 = (data: Uint8Array, value = 0) => zlib.crc32(data, value);

/** MS-DOS date and time (local time, 2 s steps), as ZIP headers keep them. */
function dosTime(d: Date) {
  const year = Math.max(1980, d.getFullYear());
  return {
    time: (d.getHours() << 11) | (d.getMinutes() << 5) | Math.floor(d.getSeconds() / 2),
    date: ((year - 1980) << 9) | ((d.getMonth() + 1) << 5) | d.getDate(),
  };
}

interface Written { name: Buffer; flags: number; crc: number; compressed: number; size: number; offset: number; method: number; zip64: boolean; time: number; date: number }

export class ZipWriter {
  private fd: fsp.FileHandle | null = null;
  private offset = 0;
  private entries: Written[] = [];
  private readonly partial: string;

  constructor(private file: string, private o: { level?: number } = {}) {
    this.partial = `${file}.partial`;
  }

  private async out() {
    if (!this.fd) { await fsp.rm(this.partial, { force: true }); this.fd = await fsp.open(this.partial, 'w'); }
    return this.fd;
  }

  private async write(buf: Buffer) {
    const fd = await this.out();
    let done = 0;
    while (done < buf.length) {
      const { bytesWritten } = await fd.write(buf, done, buf.length - done, this.offset + done);
      done += bytesWritten;
    }
    this.offset += buf.length;
  }

  /** A small entry from memory: sizes known up front, no data descriptor. */
  async addBuffer(name: string, data: Uint8Array | string, mtime = new Date()) {
    const raw = typeof data === 'string' ? Buffer.from(data, 'utf8') : Buffer.from(data);
    const packed = zlib.deflateRawSync(raw, { level: this.o.level ?? 6 });
    const method = packed.length < raw.length ? DEFLATE : STORE;
    const body = method === DEFLATE ? packed : raw;
    const nameBuf = Buffer.from(name, 'utf8');
    const { time, date } = dosTime(mtime);
    const crc = crc32(raw);
    const zip64 = raw.length >= MAX32 || this.offset >= MAX32;
    const entry: Written = { name: nameBuf, flags: FLAG_UTF8, crc, compressed: body.length, size: raw.length, offset: this.offset, method, zip64, time, date };
    const extra = zip64 ? zip64Extra([raw.length, body.length]) : Buffer.alloc(0);
    const h = Buffer.alloc(30);
    h.writeUInt32LE(SIG_LOCAL, 0); h.writeUInt16LE(zip64 ? 45 : 20, 4); h.writeUInt16LE(FLAG_UTF8, 6);
    h.writeUInt16LE(method, 8); h.writeUInt16LE(time, 10); h.writeUInt16LE(date, 12); h.writeUInt32LE(crc, 14);
    h.writeUInt32LE(zip64 ? MAX32 : body.length, 18); h.writeUInt32LE(zip64 ? MAX32 : raw.length, 22);
    h.writeUInt16LE(nameBuf.length, 26); h.writeUInt16LE(extra.length, 28);
    await this.write(Buffer.concat([h, nameBuf, extra, body]));
    this.entries.push(entry);
  }

  /**
   * A file, streamed and deflated: its sizes and CRC follow it in a ZIP64 data descriptor, so a file of any size
   * goes through without being read twice. `onBytes` reports the bytes read so far (progress).
   */
  async addFile(name: string, file: string, onBytes?: (read: number) => void) {
    const stat = await fsp.stat(file);
    const nameBuf = Buffer.from(name, 'utf8');
    const { time, date } = dosTime(stat.mtime);
    const extra = zip64Extra([0, 0]);
    const h = Buffer.alloc(30);
    h.writeUInt32LE(SIG_LOCAL, 0); h.writeUInt16LE(45, 4); h.writeUInt16LE(FLAG_UTF8 | FLAG_DESCRIPTOR, 6);
    h.writeUInt16LE(DEFLATE, 8); h.writeUInt16LE(time, 10); h.writeUInt16LE(date, 12);
    h.writeUInt32LE(0, 14); h.writeUInt32LE(MAX32, 18); h.writeUInt32LE(MAX32, 22);
    h.writeUInt16LE(nameBuf.length, 26); h.writeUInt16LE(extra.length, 28);
    const offset = this.offset;
    await this.write(Buffer.concat([h, nameBuf, extra]));

    let crc = 0, size = 0, compressed = 0;
    const self = this;
    const hash = new Transform({
      transform(chunk: Buffer, _enc, cb) { crc = crc32(chunk, crc); size += chunk.length; onBytes?.(size); cb(null, chunk); },
    });
    const sink = new Transform({
      transform(chunk: Buffer, _enc, cb) { compressed += chunk.length; self.write(chunk).then(() => cb(), cb); },
    });
    await pipeline(fs.createReadStream(file, { highWaterMark: 1 << 20 }), hash, zlib.createDeflateRaw({ level: this.o.level ?? 6 }), sink);

    const d = Buffer.alloc(24);
    d.writeUInt32LE(SIG_DESCRIPTOR, 0); d.writeUInt32LE(crc, 4);
    d.writeBigUInt64LE(BigInt(compressed), 8); d.writeBigUInt64LE(BigInt(size), 16);
    await this.write(d);
    this.entries.push({ name: nameBuf, flags: FLAG_UTF8 | FLAG_DESCRIPTOR, crc, compressed, size, offset, method: DEFLATE, zip64: true, time, date });
  }

  /** The central directory and the end records; then the archive takes its real name. */
  async finish() {
    const cdStart = this.offset;
    for (const e of this.entries) {
      const big = e.zip64 || e.size >= MAX32 || e.compressed >= MAX32 || e.offset >= MAX32;
      const extra = big ? zip64Extra([e.size, e.compressed, e.offset]) : Buffer.alloc(0);
      const c = Buffer.alloc(46);
      c.writeUInt32LE(SIG_CENTRAL, 0); c.writeUInt16LE(45, 4); c.writeUInt16LE(big ? 45 : 20, 6);
      c.writeUInt16LE(e.flags, 8);
      c.writeUInt16LE(e.method, 10); c.writeUInt16LE(e.time, 12); c.writeUInt16LE(e.date, 14); c.writeUInt32LE(e.crc, 16);
      c.writeUInt32LE(big ? MAX32 : e.compressed, 20); c.writeUInt32LE(big ? MAX32 : e.size, 24);
      c.writeUInt16LE(e.name.length, 28); c.writeUInt16LE(extra.length, 30);
      c.writeUInt32LE(big ? MAX32 : e.offset, 42);
      await this.write(Buffer.concat([c, e.name, extra]));
    }
    const cdSize = this.offset - cdStart;
    const n = this.entries.length;
    const needs64 = this.entries.some((e) => e.zip64) || n >= MAX16 || cdStart >= MAX32 || cdSize >= MAX32;
    if (needs64) {
      const eocd64At = this.offset;
      const r = Buffer.alloc(56);
      r.writeUInt32LE(SIG_EOCD64, 0); r.writeBigUInt64LE(44n, 4); r.writeUInt16LE(45, 12); r.writeUInt16LE(45, 14);
      r.writeBigUInt64LE(BigInt(n), 24); r.writeBigUInt64LE(BigInt(n), 32);
      r.writeBigUInt64LE(BigInt(cdSize), 40); r.writeBigUInt64LE(BigInt(cdStart), 48);
      const l = Buffer.alloc(20);
      l.writeUInt32LE(SIG_LOCATOR64, 0); l.writeBigUInt64LE(BigInt(eocd64At), 8); l.writeUInt32LE(1, 16);
      await this.write(Buffer.concat([r, l]));
    }
    const e = Buffer.alloc(22);
    e.writeUInt32LE(SIG_EOCD, 0);
    e.writeUInt16LE(Math.min(n, MAX16), 8); e.writeUInt16LE(Math.min(n, MAX16), 10);
    e.writeUInt32LE(needs64 ? MAX32 : cdSize, 12); e.writeUInt32LE(needs64 ? MAX32 : cdStart, 16);
    await this.write(e);
    const fd = await this.out();
    await fd.sync();
    await fd.close();
    this.fd = null;
    await fsp.rename(this.partial, this.file);
    return { file: this.file, bytes: this.offset };
  }

  /** Stops and deletes what was written (a cancelled or failed export leaves nothing behind). */
  async abort() {
    try { await this.fd?.close(); } catch { /* already closed */ }
    this.fd = null;
    await fsp.rm(this.partial, { force: true });
  }
}

/** The ZIP64 extended information extra field (0x0001) holding these 8-byte values. */
function zip64Extra(values: number[]) {
  const b = Buffer.alloc(4 + 8 * values.length);
  b.writeUInt16LE(0x0001, 0); b.writeUInt16LE(8 * values.length, 2);
  values.forEach((v, i) => b.writeBigUInt64LE(BigInt(v), 4 + 8 * i));
  return b;
}

// ------------------------------------------------------------------ reading

export interface ZipEntry { name: string; method: number; crc: number; compressed: number; size: number; offset: number }

/** The archive's entries, from its central directory. Throws a readable message when it is not a ZIP file. */
export async function readZip(file: string): Promise<ZipEntry[]> {
  const fd = await fsp.open(file, 'r');
  try {
    const { size } = await fd.stat();
    const read = async (pos: number, len: number) => {
      const b = Buffer.alloc(len);
      const { bytesRead } = await fd.read(b, 0, len, pos);
      return b.subarray(0, bytesRead);
    };
    const tailLen = Math.min(size, 22 + MAX16 + 20);
    const tail = await read(size - tailLen, tailLen);
    let at = -1;
    for (let i = tail.length - 22; i >= 0; i--) if (tail.readUInt32LE(i) === SIG_EOCD) { at = i; break; }
    if (at < 0) throw new Error('This file is not a ZIP archive (or it is incomplete: still downloading or syncing?).');
    let count = tail.readUInt16LE(at + 10);
    let cdSize = tail.readUInt32LE(at + 12);
    let cdStart = tail.readUInt32LE(at + 16);
    const locAt = at - 20;
    if (locAt >= 0 && tail.readUInt32LE(locAt) === SIG_LOCATOR64) {
      const r = await read(Number(tail.readBigUInt64LE(locAt + 8)), 56);
      if (r.readUInt32LE(0) !== SIG_EOCD64) throw new Error('The ZIP archive is damaged (ZIP64 end record).');
      count = Number(r.readBigUInt64LE(32));
      cdSize = Number(r.readBigUInt64LE(40));
      cdStart = Number(r.readBigUInt64LE(48));
    }
    const cd = await read(cdStart, cdSize);
    const entries: ZipEntry[] = [];
    let p = 0;
    for (let i = 0; i < count; i++) {
      if (cd.readUInt32LE(p) !== SIG_CENTRAL) throw new Error('The ZIP archive is damaged (central directory).');
      const method = cd.readUInt16LE(p + 10), crc = cd.readUInt32LE(p + 16);
      let compressed = cd.readUInt32LE(p + 20), sz = cd.readUInt32LE(p + 24);
      const nameLen = cd.readUInt16LE(p + 28), extraLen = cd.readUInt16LE(p + 30), commentLen = cd.readUInt16LE(p + 32);
      let offset = cd.readUInt32LE(p + 42);
      const name = cd.subarray(p + 46, p + 46 + nameLen).toString('utf8');
      const extra = cd.subarray(p + 46 + nameLen, p + 46 + nameLen + extraLen);
      for (let x = 0; x + 4 <= extra.length;) {
        const id = extra.readUInt16LE(x), len = extra.readUInt16LE(x + 2);
        if (id === 0x0001) {
          let y = x + 4;
          if (sz === MAX32) { sz = Number(extra.readBigUInt64LE(y)); y += 8; }
          if (compressed === MAX32) { compressed = Number(extra.readBigUInt64LE(y)); y += 8; }
          if (offset === MAX32) { offset = Number(extra.readBigUInt64LE(y)); }
        }
        x += 4 + len;
      }
      if (method !== DEFLATE && method !== STORE) throw new Error(`Unsupported compression in ${name}.`);
      entries.push({ name, method, crc, compressed, size: sz, offset });
      p += 46 + nameLen + extraLen + commentLen;
    }
    return entries;
  } finally {
    await fd.close();
  }
}

async function dataStart(file: string, e: ZipEntry) {
  const fd = await fsp.open(file, 'r');
  try {
    const h = Buffer.alloc(30);
    await fd.read(h, 0, 30, e.offset);
    if (h.readUInt32LE(0) !== SIG_LOCAL) throw new Error(`The ZIP archive is damaged (${e.name}).`);
    return e.offset + 30 + h.readUInt16LE(26) + h.readUInt16LE(28);
  } finally {
    await fd.close();
  }
}

/** Writes one entry to `dest` (streamed), checking its size and CRC-32. `onBytes`: bytes written so far. */
export async function extractEntry(file: string, e: ZipEntry, dest: string, onBytes?: (n: number) => void) {
  const start = await dataStart(file, e);
  let crc = 0, n = 0;
  const check = new Transform({ transform(chunk: Buffer, _enc, cb) { crc = crc32(chunk, crc); n += chunk.length; onBytes?.(n); cb(null, chunk); } });
  if (e.compressed === 0) await fsp.writeFile(dest, '');
  else if (e.method === DEFLATE) {
    await pipeline(fs.createReadStream(file, { start, end: start + e.compressed - 1, highWaterMark: 1 << 20 }), zlib.createInflateRaw(), check, fs.createWriteStream(dest));
  } else {
    await pipeline(fs.createReadStream(file, { start, end: start + e.compressed - 1, highWaterMark: 1 << 20 }), check, fs.createWriteStream(dest));
  }
  if (n !== e.size || crc !== e.crc) throw new Error(`${e.name} in the ZIP archive is damaged (its check failed): download it again.`);
}

/** One small entry into memory (the manifest), checked. */
export async function readEntry(file: string, e: ZipEntry): Promise<Buffer> {
  if (e.size > 64 << 20) throw new Error(`${e.name} is too large to read into memory.`);
  const start = await dataStart(file, e);
  const fd = await fsp.open(file, 'r');
  try {
    const b = Buffer.alloc(e.compressed);
    if (e.compressed) await fd.read(b, 0, e.compressed, start);
    const raw = e.method === DEFLATE ? zlib.inflateRawSync(b) : b;
    if (raw.length !== e.size || crc32(raw) !== e.crc) throw new Error(`${e.name} in the ZIP archive is damaged: download it again.`);
    return raw;
  } finally {
    await fd.close();
  }
}
