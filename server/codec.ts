// Compressed blobs (0.14.0): the Riot API cache (riot/diskCache.ts) and the database's timelines (db.ts, data_z).
// Each blob is a zstd frame at level 9 with a raw-content dictionary: a typical example of what that database holds
// (for the cache, a match and a timeline as Riot sends them; for the database, a timeline capture), taken from the
// data folder itself, so it fits the data it compresses. Riot-shaped JSON (synthetic, docs/releases/0.14.1.md): a match
// is 35 % and a timeline 21 % smaller than gzip, and both decode as fast. Blobs written at 0.14.0's level 15 read the
// same: the level is the encoder's alone.
//
//   blob = [1 = zstd][dictionary id, uint16 little-endian][zstd frame]      dictionary 0: none
//
// A blob names its dictionary, and a database keeps every dictionary it has used (codec_dicts): a later version can
// build a better one without touching the rows written with an older one.
import type { DatabaseSync } from 'node:sqlite';
import zlib from 'node:zlib';

const FORMAT_ZSTD = 1;
const HEADER = 3;
/**
 * zstd's level (0.14.1). With a dictionary, level 15 took 37 ms a match and 137 ms a timeline of one core (hours to
 * convert a large cache, and about 0.17 s a fetched game); level 9 takes 9.5 and 21 ms for 16 and 13 % more bytes.
 */
export const LEVEL = 9;
/** How much of each typical example goes into a dictionary. */
const SAMPLE_BYTES = 64 * 1024;

export interface Dictionary { id: number; bytes: Buffer }

/** `raw` compressed with `dict` (null: none). */
export function pack(raw: Uint8Array, dict: Dictionary | null): Buffer {
  const frame = zlib.zstdCompressSync(raw, {
    params: { [zlib.constants.ZSTD_c_compressionLevel]: LEVEL, [zlib.constants.ZSTD_c_checksumFlag]: 1 },
    ...(dict ? { dictionary: dict.bytes } : {}),
  });
  const out = Buffer.allocUnsafe(HEADER + frame.length);
  out[0] = FORMAT_ZSTD;
  out.writeUInt16LE(dict?.id ?? 0, 1);
  frame.copy(out, HEADER);
  return out;
}

/** The dictionary id a blob was written with. */
export const dictionaryOf = (blob: Uint8Array) => blob[1] | (blob[2] << 8);

/** The bytes `pack` was given. Throws on a blob it can't read (unknown format, a dictionary `dicts` doesn't have, corruption). */
export function unpack(blob: Uint8Array, dicts: (id: number) => Buffer | undefined): Buffer {
  if (blob.length < HEADER || blob[0] !== FORMAT_ZSTD) throw new Error('Not a compressed blob');
  const id = dictionaryOf(blob);
  const dictionary = id ? dicts(id) : undefined;
  if (id && !dictionary) throw new Error(`Compressed with dictionary ${id}, which this database doesn't have`);
  const frame = Buffer.from(blob.buffer, blob.byteOffset + HEADER, blob.length - HEADER);
  return zlib.zstdDecompressSync(frame, dictionary ? { dictionary } : {});
}

/** A raw-content dictionary from typical examples (the first 64 KB of each). */
export const dictionaryFrom = (samples: Uint8Array[]) => Buffer.concat(samples.map((s) => Buffer.from(s.buffer, s.byteOffset, Math.min(s.length, SAMPLE_BYTES))));

export const CODEC_SCHEMA = `
CREATE TABLE IF NOT EXISTS codec_dicts (
  id         INTEGER PRIMARY KEY,
  bytes      BLOB NOT NULL,
  made_at    INTEGER NOT NULL,
  made_from  TEXT NOT NULL               -- what the examples were (match ids)
);`;

/**
 * The dictionaries of one database (its codec_dicts table): the newest is the one new blobs are written with. Another
 * connection may add one meanwhile: an id this one doesn't know is read again from the table.
 */
export class Dictionaries {
  private known = new Map<number, Buffer>();
  private newest: Dictionary | null = null;

  constructor(private db: DatabaseSync) { this.reload(); }

  private reload() {
    let rows: { id: number; bytes: Uint8Array }[];
    // A database from before 0.14.0 opened without its migration has none.
    try { rows = this.db.prepare('SELECT id, bytes FROM codec_dicts ORDER BY id').all() as typeof rows; } catch { return; }
    for (const r of rows) this.known.set(r.id, Buffer.from(r.bytes));
    const last = rows.at(-1);
    this.newest = last ? { id: last.id, bytes: this.known.get(last.id)! } : null;
  }

  get = (id: number): Buffer | undefined => {
    if (!this.known.has(id)) this.reload();
    return this.known.get(id);
  };

  /** Every dictionary this connection knows, by id (handed to threads that only read and write blobs). */
  all(): [number, Buffer][] { this.reload(); return [...this.known]; }

  /** The dictionary new blobs are written with (null: none made yet). */
  current(): Dictionary | null {
    return this.newest;
  }

  /** Makes one from typical examples and makes it the current one (or takes one another connection just made). */
  create(samples: Uint8Array[], madeFrom: string): Dictionary {
    this.reload();
    if (this.newest) return this.newest;
    const bytes = dictionaryFrom(samples);
    this.db.prepare('INSERT INTO codec_dicts(id, bytes, made_at, made_from) VALUES(1, ?, ?, ?) ON CONFLICT(id) DO NOTHING').run(bytes, Date.now(), madeFrom);
    this.reload();
    return this.newest!;
  }
}
