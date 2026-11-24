import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import zlib from 'node:zlib';
import type { ParticipantRow } from './capture.ts';
import { CODEC_SCHEMA, Dictionaries, pack, unpack } from './codec.ts';
import type { ModelGameRow, ParticipantLite } from './model/dataset.ts';
import type { ChampionTotals } from './model/profiles.ts';

// Schema
// ------
// players               high-elo accounts from league-v4 (plus Irelia players harvested from matches); pilot
//                       class and Irelia activity from champion-mastery-v4
// matches               every match id the scraper has looked at (dedupe), with its patch and quality flags
// irelia_games          one row per Irelia game: the raw facts the build engine aggregates, tagged with its source.
//                       Purchases and skill order are stored raw so item classification is re-evaluated
//                       against the *current* patch's item data at recommendation time (patch-aware).
// participants          v3: every player of every stored match (damage mix, healing, CC, items)
// timelines             v3: compact per-minute timeline of each Irelia game (server/capture.ts TimelineCapture)
// game_versions         v3: when each game build was first and last seen per platform (patch per region, hotfixes)
// champion_patch_stats  v3: per-champion totals kept for patches whose raw games were purged
// match_queue           v3: matches (and harvested games' timelines) waiting for the regional host
// rec_log               v4: builds the app showed you (champ select, in game) and, once your game is fetched, what you
//                       bought and how it went (personal layer, model/personal.ts)
// archive               v5: matches of patches older than patch memory's window, each one compressed whole and out of the
//                       tables above (Archive old patches); Restore puts them back
// codec_dicts           v6: the dictionaries timelines.data_z is compressed with (server/codec.ts)
// Details and the reasons for rows vs blobs: docs/BUILD-SOURCING.md §Schema.
export const SCHEMA_VERSION = 6;

/**
 * Every connection's settings (the busy timeout is set per connection: openDb). synchronous = NORMAL (0.8.6), WAL's
 * recommended mode: a commit no longer waits for the disk (the WAL is flushed at each checkpoint). With FULL every
 * commit held the write lock through a flush, tens of milliseconds on some Windows disks, so a thread writing often
 * could keep the lock nearly all the time and another's write timed out waiting for it. An app crash loses nothing;
 * a power cut can roll back the last commits, never corrupt the database.
 */
const PRAGMAS = `
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA journal_size_limit = 67108864;
`;

/** How long a connection waits for another one's write to finish before "database is locked" (openDb `busyTimeoutMs`). */
export const BUSY_TIMEOUT_MS = 5000;

const SCHEMA = `
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS players (
  puuid            TEXT PRIMARY KEY,
  platform         TEXT NOT NULL,
  tier             TEXT NOT NULL,
  league_points    INTEGER NOT NULL DEFAULT 0,
  is_otp           INTEGER,                -- NULL = not checked yet
  irelia_mastery   INTEGER,
  last_checked     INTEGER,                -- epoch ms of mastery check
  last_match_scan  INTEGER,                -- epoch ms of match-id scan
  updated_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_players_otp ON players(is_otp, last_match_scan);

CREATE TABLE IF NOT EXISTS matches (
  match_id       TEXT PRIMARY KEY,
  platform       TEXT NOT NULL,
  patch          TEXT NOT NULL,            -- "16.19"
  game_version   TEXT NOT NULL,            -- "16.19.712.4471"
  queue_id       INTEGER NOT NULL,
  game_creation  INTEGER NOT NULL,
  game_duration  INTEGER NOT NULL,
  has_irelia     INTEGER NOT NULL,
  fetched_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_matches_patch ON matches(patch);

CREATE TABLE IF NOT EXISTS irelia_games (
  match_id              TEXT PRIMARY KEY REFERENCES matches(match_id) ON DELETE CASCADE,
  puuid                 TEXT NOT NULL,
  patch                 TEXT NOT NULL,
  win                   INTEGER NOT NULL,
  position              TEXT NOT NULL,
  opponent_champion_id  INTEGER,
  summoner1             INTEGER NOT NULL,
  summoner2             INTEGER NOT NULL,
  primary_style         INTEGER NOT NULL,
  sub_style             INTEGER NOT NULL,
  perks                 TEXT NOT NULL,     -- JSON [6]
  stat_perks            TEXT NOT NULL,     -- JSON [offense, flex, defense]
  skill_order           TEXT NOT NULL,     -- "QEWQQRQ..."
  purchases             TEXT NOT NULL,     -- JSON [[itemId, timestampMs], ...] after undo resolution
  final_items           TEXT NOT NULL,     -- JSON [item0..item6]
  kills INTEGER NOT NULL, deaths INTEGER NOT NULL, assists INTEGER NOT NULL,
  game_duration         INTEGER NOT NULL,
  created_at            INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_irelia_patch_opp ON irelia_games(patch, opponent_champion_id);
-- The newest stored game, read with every data-version check (dataVersion): without it, a scan of every game.
CREATE INDEX IF NOT EXISTS idx_irelia_created ON irelia_games(created_at);

-- v2: platform ids are stored upper-case ("KR", "EUW1") to match routing.ts
UPDATE players SET platform = UPPER(platform) WHERE platform <> UPPER(platform);
UPDATE matches SET platform = UPPER(platform) WHERE platform <> UPPER(platform);
CREATE INDEX IF NOT EXISTS idx_players_platform ON players(platform, is_otp);

-- v3: build sourcing v2 (docs/BUILD-SOURCING.md)
-- participants: PARTICIPANTS_TABLE below (a table, or since 0.14.0 maybe a view over participant_rows)

CREATE TABLE IF NOT EXISTS timelines (
  match_id        TEXT PRIMARY KEY REFERENCES matches(match_id) ON DELETE CASCADE,
  frame_interval  INTEGER NOT NULL,
  data            TEXT NOT NULL           -- JSON TimelineCapture (server/capture.ts)
);

CREATE TABLE IF NOT EXISTS game_versions (
  platform      TEXT NOT NULL,
  game_version  TEXT NOT NULL,            -- full build, "16.19.712.4471": a hotfix may add a new one mid-patch
  patch         TEXT NOT NULL,
  first_seen    INTEGER NOT NULL,         -- earliest game_creation seen (epoch ms)
  last_seen     INTEGER NOT NULL,
  games         INTEGER NOT NULL,
  PRIMARY KEY (platform, game_version)
);

CREATE TABLE IF NOT EXISTS champion_patch_stats (
  patch TEXT NOT NULL, champion_id INTEGER NOT NULL, position TEXT NOT NULL,
  games INTEGER NOT NULL, wins INTEGER NOT NULL, seconds INTEGER NOT NULL,
  dmg_physical INTEGER NOT NULL, dmg_magic INTEGER NOT NULL, dmg_true INTEGER NOT NULL,
  dmg_taken INTEGER NOT NULL, dmg_mitigated INTEGER NOT NULL,
  heal INTEGER NOT NULL, heal_allies INTEGER NOT NULL, shield_allies INTEGER NOT NULL,
  cc_time INTEGER NOT NULL,
  PRIMARY KEY (patch, champion_id, position)
);

CREATE TABLE IF NOT EXISTS match_queue (
  match_id   TEXT PRIMARY KEY,
  platform   TEXT NOT NULL,
  kind       TEXT NOT NULL,               -- 'match' (fetch it) | 'timeline' (harvested game: match cached, timeline needed)
  puuid      TEXT NOT NULL,               -- the pilot whose history listed it, or the harvested Irelia
  source     TEXT NOT NULL,               -- otp | main | harvested | me
  score      REAL NOT NULL,               -- expected Irelia games per API call
  added_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_match_queue_next ON match_queue(platform, score DESC);

-- v4: draft-aware builds (docs/BUILD-SOURCING.md §9.4)
CREATE TABLE IF NOT EXISTS rec_log (
  id           INTEGER PRIMARY KEY,
  session      TEXT NOT NULL UNIQUE,        -- 'draft:<champ select start>' | 'live:<game start>': one row per phase of a game
  phase        TEXT NOT NULL,               -- draft | live
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  lane         TEXT,
  opponent_id  INTEGER,
  mode         TEXT NOT NULL,               -- baseline | draft: which build the app showed
  shown        TEXT NOT NULL,               -- JSON: keystone, core, boots, items 4-6; in game, every "Next" item shown
  match_id     TEXT,                        -- your game, once it is fetched (source "me")
  bought       TEXT,                        -- JSON: what you bought (completed items in order, keystone)
  win          INTEGER
);
CREATE INDEX IF NOT EXISTS idx_rec_log_match ON rec_log(match_id);

-- v5: patch memory's archive (docs/BUILD-SOURCING.md §9.8)
CREATE TABLE IF NOT EXISTS archive (
  match_id     TEXT PRIMARY KEY,
  patch        TEXT NOT NULL,
  archived_at  INTEGER NOT NULL,
  data         BLOB NOT NULL               -- gzip of JSON { match, game, participants, timeline }: each row as stored
);
CREATE INDEX IF NOT EXISTS idx_archive_patch ON archive(patch);

-- v6: compressed timelines (timelines.data_z, added below) and their dictionaries
${CODEC_SCHEMA}
`;

/** v3's participants table (every player of every stored match); left alone once it is a view (participantsByIds). */
const PARTICIPANTS_TABLE = `
CREATE TABLE IF NOT EXISTS participants (
  match_id        TEXT NOT NULL REFERENCES matches(match_id) ON DELETE CASCADE,
  participant_id  INTEGER NOT NULL,
  puuid           TEXT NOT NULL,
  champion_id     INTEGER NOT NULL,
  team_id         INTEGER NOT NULL,
  position        TEXT NOT NULL,          -- teamPosition
  win             INTEGER NOT NULL,
  champ_level     INTEGER NOT NULL,
  kills INTEGER NOT NULL, deaths INTEGER NOT NULL, assists INTEGER NOT NULL,
  gold_earned     INTEGER NOT NULL,
  cs              INTEGER NOT NULL,       -- lane minions + neutral monsters
  items           TEXT NOT NULL,          -- JSON [item0..item6]
  dmg_physical    INTEGER NOT NULL,       -- damage to champions, by type
  dmg_magic       INTEGER NOT NULL,
  dmg_true        INTEGER NOT NULL,
  dmg_taken       INTEGER NOT NULL,
  dmg_mitigated   INTEGER NOT NULL,
  heal            INTEGER NOT NULL,
  heal_allies     INTEGER NOT NULL,
  shield_allies   INTEGER NOT NULL,
  cc_time         INTEGER NOT NULL,       -- timeCCingOthers (s)
  cc_dealt        INTEGER NOT NULL,       -- totalTimeCCDealt (s)
  time_played     INTEGER NOT NULL,       -- s
  challenges      TEXT,                   -- JSON subset (capture.ts CHALLENGE_KEYS)
  PRIMARY KEY (match_id, participant_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_participants_champion ON participants(champion_id, position);`;

/**
 * Participants by integer ids (0.14.0, Compact database): each PUUID and match id stored once (puuid_ids, match_ids)
 * and referred to by its id in participant_rows. `participants` becomes a view with the same columns, so every reader
 * reads it as before; the writes go to participant_rows (insertParticipants, the archive's restore), and a match's
 * rows go with it by trigger, as the table's ON DELETE CASCADE did. Made only when participants and its index are over
 * a quarter of the database, and kept only when it comes out smaller (PUUIDs repeat across matches in real data).
 */
const PARTICIPANT_VALUE_COLUMNS = ['champion_id', 'team_id', 'position', 'win', 'champ_level', 'kills', 'deaths', 'assists',
  'gold_earned', 'cs', 'items', 'dmg_physical', 'dmg_magic', 'dmg_true', 'dmg_taken', 'dmg_mitigated', 'heal',
  'heal_allies', 'shield_allies', 'cc_time', 'cc_dealt', 'time_played', 'challenges'] as const;
const PARTICIPANTS_BY_IDS = `
CREATE TABLE match_ids (id INTEGER PRIMARY KEY, match_id TEXT NOT NULL UNIQUE);
CREATE TABLE puuid_ids (id INTEGER PRIMARY KEY, puuid TEXT NOT NULL UNIQUE);
INSERT INTO match_ids(match_id) SELECT DISTINCT match_id FROM participants ORDER BY match_id;
INSERT INTO puuid_ids(puuid) SELECT DISTINCT puuid FROM participants ORDER BY puuid;
CREATE TABLE participant_rows (
  match_ref       INTEGER NOT NULL,       -- match_ids.id
  participant_id  INTEGER NOT NULL,
  puuid_ref       INTEGER NOT NULL,       -- puuid_ids.id
  champion_id INTEGER NOT NULL, team_id INTEGER NOT NULL, position TEXT NOT NULL, win INTEGER NOT NULL,
  champ_level INTEGER NOT NULL, kills INTEGER NOT NULL, deaths INTEGER NOT NULL, assists INTEGER NOT NULL,
  gold_earned INTEGER NOT NULL, cs INTEGER NOT NULL, items TEXT NOT NULL,
  dmg_physical INTEGER NOT NULL, dmg_magic INTEGER NOT NULL, dmg_true INTEGER NOT NULL, dmg_taken INTEGER NOT NULL,
  dmg_mitigated INTEGER NOT NULL, heal INTEGER NOT NULL, heal_allies INTEGER NOT NULL, shield_allies INTEGER NOT NULL,
  cc_time INTEGER NOT NULL, cc_dealt INTEGER NOT NULL, time_played INTEGER NOT NULL, challenges TEXT,
  PRIMARY KEY (match_ref, participant_id)
) WITHOUT ROWID;
INSERT INTO participant_rows SELECT m.id, p.participant_id, u.id, ${PARTICIPANT_VALUE_COLUMNS.map((c) => `p.${c}`).join(', ')}
  FROM participants p JOIN match_ids m USING(match_id) JOIN puuid_ids u USING(puuid);
CREATE INDEX idx_participant_rows_champion ON participant_rows(champion_id, position);
`;
/** The view's query: the old table's columns, in its order. */
const PARTICIPANTS_VIEW_SELECT = `SELECT m.match_id AS match_id, p.participant_id AS participant_id, u.puuid AS puuid,
  ${PARTICIPANT_VALUE_COLUMNS.map((c) => `p.${c} AS ${c}`).join(', ')}
  FROM participant_rows p JOIN match_ids m ON m.id = p.match_ref LEFT JOIN puuid_ids u ON u.id = p.puuid_ref`;
const PARTICIPANTS_VIEW = `
CREATE VIEW participants AS ${PARTICIPANTS_VIEW_SELECT};
CREATE TRIGGER participants_of_deleted_match AFTER DELETE ON matches BEGIN
  DELETE FROM participant_rows WHERE match_ref = (SELECT id FROM match_ids WHERE match_id = old.match_id);
  DELETE FROM match_ids WHERE match_id = old.match_id;
END;
`;
/** Share of the database participants (and its index) must exceed for the rebuild to be tried. */
export const PARTICIPANTS_SHARE = 0.25;

/** Per-champion totals of the matches `where` selects (m: matches, p: participants; remakes excluded), as SQL. */
const championTotalsOf = (where: string) => `SELECT m.patch, p.champion_id, p.position, COUNT(*) AS games, SUM(p.win) AS wins,
    SUM(m.game_duration) AS seconds, SUM(p.dmg_physical) AS dmg_physical, SUM(p.dmg_magic) AS dmg_magic, SUM(p.dmg_true) AS dmg_true,
    SUM(p.dmg_taken) AS dmg_taken, SUM(p.dmg_mitigated) AS dmg_mitigated, SUM(p.heal) AS heal, SUM(p.heal_allies) AS heal_allies,
    SUM(p.shield_allies) AS shield_allies, SUM(p.cc_time) AS cc_time
  FROM participants p JOIN matches m USING(match_id)
  WHERE ${where} AND COALESCE(m.remake, 0) = 0
  GROUP BY m.patch, p.champion_id, p.position`;
const TOTAL_COLUMNS = ['games', 'wins', 'seconds', 'dmg_physical', 'dmg_magic', 'dmg_true', 'dmg_taken', 'dmg_mitigated', 'heal',
  'heal_allies', 'shield_allies', 'cc_time'];
/** Adds the totals of the matches `where` selects to champion_patch_stats (before they leave the working tables). */
const keepChampionStatsSql = (where: string) => `INSERT INTO champion_patch_stats(patch, champion_id, position, ${TOTAL_COLUMNS.join(', ')})
  ${championTotalsOf(where)}
  ON CONFLICT(patch, champion_id, position) DO UPDATE SET ${TOTAL_COLUMNS.map((c) => `${c} = ${c} + excluded.${c}`).join(', ')}`;
/** Matches stored whole in the archive: the tables each is spread over (the match row first). */
const ARCHIVED_TABLES = ['matches', 'irelia_games', 'participants', 'timelines'] as const;

/** Columns added to the first schema's tables in v3; ALTER TABLE has no IF NOT EXISTS, so they are added by hand. */
const ADDED_COLUMNS: Record<string, [string, string][]> = {
  players: [
    ['pilot_class', 'TEXT'],                    // otp | main | none (NULL: not classified yet)
    ['irelia_points', 'INTEGER'],               // Irelia mastery points when the mastery check saw them
    ['irelia_last_play', 'INTEGER'],            // lastPlayTime of Irelia (epoch ms)
    ['irelia_rank', 'INTEGER'],                 // Irelia's place in the player's top masteries (1 = #1)
    ['found_by', "TEXT NOT NULL DEFAULT 'ladder'"],   // ladder | harvest | me
    ['history_matches', 'INTEGER NOT NULL DEFAULT 0'],  // matches fetched from this player's history
    ['history_irelia', 'INTEGER NOT NULL DEFAULT 0'],   // ... of which were their Irelia games
    ['rank_checked', 'INTEGER'],                // league-v4 by-puuid lookup (harvested players off the ladder)
  ],
  matches: [
    ['remake', 'INTEGER'],                      // 1: remade (capture.ts matchQuality); NULL: not captured yet
    ['leaver', 'INTEGER'],                      // 1: someone left (timeline); NULL: unknown (no timeline)
    ['end_result', 'TEXT'],                     // info.endOfGameResult
    ['detail_version', 'INTEGER NOT NULL DEFAULT 0'],  // capture.ts CAPTURE_VERSION applied (0: not yet)
  ],
  timelines: [
    ['data_z', 'BLOB'],                         // v6: the capture compressed (server/codec.ts); data is '' then
  ],
  irelia_games: [
    ['participant_id', 'INTEGER'],
    ['position_opponent_id', 'INTEGER'],        // enemy with Irelia's teamPosition
    ['timeline_opponent_id', 'INTEGER'],        // enemy nearest Irelia in lane, 3:00-14:00 (NULL: no timeline yet)
    ['pilot_source', 'TEXT'],                   // otp | main | harvested | me (NULL: stored before v3, all OTP games)
    ['pilot_tier', 'TEXT'],
    ['pilot_lp', 'INTEGER'],
    ['pilot_mastery', 'INTEGER'],
    ['pilot_last_play', 'INTEGER'],
    ['pilot_top1', 'INTEGER'],                  // 1: Irelia is the pilot's #1 mastery champion
  ],
};

const MIGRATE_V3 = `
CREATE INDEX IF NOT EXISTS idx_players_pilot ON players(platform, pilot_class, irelia_last_play);
CREATE INDEX IF NOT EXISTS idx_irelia_source ON irelia_games(pilot_source, patch);
CREATE INDEX IF NOT EXISTS idx_matches_detail ON matches(detail_version);
-- Every game stored before v3 came from an OTP's own history.
UPDATE irelia_games SET pilot_source = 'otp' WHERE pilot_source IS NULL;
UPDATE irelia_games SET position_opponent_id = opponent_champion_id WHERE position_opponent_id IS NULL AND timeline_opponent_id IS NULL;
UPDATE players SET pilot_class = CASE WHEN is_otp = 1 THEN 'otp' ELSE 'none' END WHERE pilot_class IS NULL AND is_otp IS NOT NULL;
`;

/** Where an Irelia game came from. 'me' is the user's own account and never feeds the high-elo builds. */
export type PilotSource = 'otp' | 'main' | 'harvested' | 'me';
export const PILOT_SOURCES: readonly PilotSource[] = ['otp', 'main', 'harvested', 'me'];

export interface IreliaGameRow {
  match_id: string;
  puuid: string;
  patch: string;
  win: number;
  position: string;
  opponent_champion_id: number | null;   // the lane opponent the engine uses: timeline's when known, else the position's
  summoner1: number;
  summoner2: number;
  primary_style: number;
  sub_style: number;
  perks: string;
  stat_perks: string;
  skill_order: string;
  purchases: string;
  final_items: string;
  kills: number;
  deaths: number;
  assists: number;
  game_duration: number;
  created_at: number;
  // v3
  participant_id?: number | null;
  position_opponent_id?: number | null;
  timeline_opponent_id?: number | null;
  pilot_source?: PilotSource | null;
  pilot_tier?: string | null;
  pilot_lp?: number | null;
  pilot_mastery?: number | null;
  pilot_last_play?: number | null;
  pilot_top1?: number | null;
  // joined from matches when the engine reads games
  remake?: number | null;
  leaver?: number | null;
  end_result?: string | null;
  game_version?: string;
}

const GAME_COLUMNS = [
  'match_id', 'puuid', 'patch', 'win', 'position', 'opponent_champion_id', 'summoner1', 'summoner2', 'primary_style',
  'sub_style', 'perks', 'stat_perks', 'skill_order', 'purchases', 'final_items', 'kills', 'deaths', 'assists',
  'game_duration', 'created_at', 'participant_id', 'position_opponent_id', 'timeline_opponent_id', 'pilot_source',
  'pilot_tier', 'pilot_lp', 'pilot_mastery', 'pilot_last_play', 'pilot_top1',
] as const;

const PARTICIPANT_COLUMNS = [
  'match_id', 'participant_id', 'puuid', 'champion_id', 'team_id', 'position', 'win', 'champ_level', 'kills', 'deaths',
  'assists', 'gold_earned', 'cs', 'items', 'dmg_physical', 'dmg_magic', 'dmg_true', 'dmg_taken', 'dmg_mitigated',
  'heal', 'heal_allies', 'shield_allies', 'cc_time', 'cc_dealt', 'time_played', 'challenges',
] as const satisfies readonly (keyof ParticipantRow)[];

export interface PlayerRow {
  puuid: string;
  platform: string;
  irelia_mastery: number | null;
  tier: string;
  league_points: number;
  is_otp: number | null;
  last_checked: number | null;
  last_match_scan: number | null;
  // v3
  pilot_class?: 'otp' | 'main' | 'none' | null;
  irelia_points?: number | null;
  irelia_last_play?: number | null;
  irelia_rank?: number | null;
  found_by?: 'ladder' | 'harvest' | 'me';
  history_matches?: number;
  history_irelia?: number;
  rank_checked?: number | null;
}

/** A pilot's mastery-based classification (scraper/pilots.ts classifyPilot). */
export interface PilotUpdate {
  pilotClass: 'otp' | 'main' | 'none';
  ireliaPoints: number;          // 0 when Irelia is not among the masteries seen
  ireliaLastPlay: number | null;
  ireliaRank: number | null;
}

export interface MatchRow { id: string; platform: string; patch: string; version: string; queue: number; creation: number; duration: number; hasIrelia: boolean }

/** One phase of one of your games in rec_log. */
export interface RecLogRow {
  id: number; session: string; phase: 'draft' | 'live'; created_at: number; updated_at: number;
  lane: string | null; opponent_id: number | null; mode: string; shown: string;
  match_id: string | null; bought: string | null; win: number | null;
}

export interface QueuedMatch { match_id: string; platform: string; kind: 'match' | 'timeline'; puuid: string; source: PilotSource; score: number }

/** One match whole, as the archive and the 0.18.0 copies keep it: its rows in each table (the timeline's data as JSON). */
export type MatchRecord = Record<'matches' | 'irelia_games' | 'participants' | 'timelines', Record<string, unknown>[]>;

const json = (v: unknown) => JSON.stringify(v);

/** SQL predicate on `g` (irelia_games) for the build sources: games stored before v3 count as OTP games. */
const SOURCE_FILTER = "COALESCE(g.pilot_source, 'otp') IN (SELECT value FROM json_each(?))";
/** The high-elo build's default sources (scrape.config.json buildSources). 'me' is never one of them. */
export const DEFAULT_BUILD_SOURCES: PilotSource[] = ['otp'];

/** Whether participants is the view over participant_rows (0.14.0 normalizeParticipants). */
const participantsByIds = (db: DatabaseSync) =>
  (db.prepare("SELECT type FROM sqlite_master WHERE name = 'participants'").get() as { type: string } | undefined)?.type === 'view';

function addColumns(db: DatabaseSync) {
  for (const [table, cols] of Object.entries(ADDED_COLUMNS)) {
    const have = new Set((db.prepare(`PRAGMA table_info(${table})`).all() as { name: string }[]).map((c) => c.name));
    for (const [name, decl] of cols) if (!have.has(name)) db.exec(`ALTER TABLE ${table} ADD COLUMN ${name} ${decl}`);
  }
}

/** Opens (and creates or upgrades in place) the database. Read-only tools (server/audit.ts) open SQLite directly. */
/**
 * Opens (and migrates) the database. `migrate: false` is for threads that start after the main thread opened and
 * migrated it (fetching, the model and the jobs): they write nothing on open, so they never wait on a write lock then.
 * `busyTimeoutMs`: how long a write waits for another connection's (the threads wait longer than the main one, whose
 * waits block the dashboard and the overlay).
 */
/**
 * Rows as plain objects (0.19.0): node:sqlite returns each row with its columns in a dictionary (about 2.1 KB a game
 * row on the test database); a copy has one shape for all of them and takes about 0.74 KB. The same values.
 */
export function compactRows<T extends object>(rows: T[]): T[] {
  for (let i = 0; i < rows.length; i++) rows[i] = { ...rows[i] };
  return rows;
}

export function openDb(file: string, o: { migrate?: boolean; busyTimeoutMs?: number } = {}) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const db = new DatabaseSync(file);
  db.exec(`PRAGMA busy_timeout = ${Math.max(0, Math.round(o.busyTimeoutMs ?? BUSY_TIMEOUT_MS))};`);
  // Space comes back by itself (0.14.0): set before the first table of a new file; an older file takes it with its
  // next full VACUUM (Settings → Storage → Compact database).
  if (o.migrate !== false) db.exec('PRAGMA auto_vacuum = INCREMENTAL;');
  db.exec(PRAGMAS);
  if (o.migrate !== false) {
    // The schema's v2 fixes are writes: the write lock first, so another thread's write can't make this fail.
    db.exec('BEGIN IMMEDIATE');
    try {
      db.exec(SCHEMA);
      if (!participantsByIds(db)) db.exec(PARTICIPANTS_TABLE);
      addColumns(db);
      db.exec(MIGRATE_V3);
      db.prepare("INSERT INTO meta(key, value) VALUES('schema_version', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .run(String(SCHEMA_VERSION));
      db.exec('COMMIT');
    } catch (err) { db.exec('ROLLBACK'); throw err; }
  }

  // A timeline as JSON, from either column (0.14.0): data, or data_z compressed with one of codec_dicts. Every query
  // that reads timelines reads them through timeline_json(data, data_z).
  const dicts = new Dictionaries(db);
  db.function('timeline_json', { deterministic: true }, (data, z) =>
    (z instanceof Uint8Array ? unpack(z, dicts.get).toString('utf8') : (data as string | null)));
  /** A capture compressed with the database's dictionary (made from this capture when there is none yet). */
  const packTimeline = (json: string) => {
    const raw = Buffer.from(json);
    return pack(raw, dicts.current() ?? dicts.create([raw], 'a timeline capture of this database'));
  };

  const byIds = participantsByIds(db);
  const q = {
    getMeta: db.prepare('SELECT value FROM meta WHERE key = ?'),
    setMeta: db.prepare('INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value'),
    upsertPlayer: db.prepare(`
      INSERT INTO players(puuid, platform, tier, league_points, updated_at) VALUES(?, ?, ?, ?, ?)
      ON CONFLICT(puuid) DO UPDATE SET tier = excluded.tier, league_points = excluded.league_points, updated_at = excluded.updated_at`),
    // Harvested and own accounts: never overwrite a ladder tier with "UNKNOWN".
    addCandidate: db.prepare(`
      INSERT INTO players(puuid, platform, tier, league_points, updated_at, found_by) VALUES(?, ?, 'UNKNOWN', 0, ?, ?)
      ON CONFLICT(puuid) DO NOTHING`),
    setRank: db.prepare('UPDATE players SET tier = ?, league_points = ?, rank_checked = ?, updated_at = ? WHERE puuid = ?'),
    getPlayer: db.prepare('SELECT * FROM players WHERE puuid = ?'),
    setPilot: db.prepare(`UPDATE players SET is_otp = ?, irelia_mastery = ?, last_checked = ?, pilot_class = ?, irelia_points = ?,
      irelia_last_play = ?, irelia_rank = ? WHERE puuid = ?`),
    forgetActivityChecks: db.prepare(`UPDATE players SET last_checked = 0 WHERE platform = ? AND pilot_class IN ('otp', 'main')
      AND last_checked >= ? AND (irelia_last_play IS NULL OR irelia_last_play < ?)`),
    duePilots: db.prepare(`
      SELECT * FROM players WHERE platform = ? AND pilot_class IN (SELECT value FROM json_each(?))
        AND (last_match_scan IS NULL OR last_match_scan < ?) AND (irelia_last_play IS NULL OR irelia_last_play >= ?)
      ORDER BY irelia_last_play IS NULL, irelia_last_play DESC LIMIT ?`),
    setActivity: db.prepare('UPDATE players SET irelia_points = ?, irelia_last_play = ?, irelia_rank = ? WHERE puuid = ?'),
    missingActivity: db.prepare(`SELECT * FROM players WHERE is_otp = 1 AND irelia_last_play IS NULL`),
    tagOwn: db.prepare(`UPDATE irelia_games SET pilot_source = 'me' WHERE puuid = ? AND COALESCE(pilot_source, '') <> 'me'`),
    countHistory: db.prepare('UPDATE players SET history_matches = history_matches + ?, history_irelia = history_irelia + ? WHERE puuid = ?'),
    unrankedCandidates: db.prepare(`SELECT * FROM players WHERE platform = ? AND tier = 'UNKNOWN' AND rank_checked IS NULL
      AND found_by = 'harvest' AND pilot_class IN ('otp', 'main') LIMIT ?`),
    setScanned: db.prepare('UPDATE players SET last_match_scan = ? WHERE puuid = ?'),
    hasMatch: db.prepare('SELECT 1 AS x FROM matches WHERE match_id = ?'),
    hasGame: db.prepare('SELECT pilot_source FROM irelia_games WHERE match_id = ?'),
    insertMatch: db.prepare(`
      INSERT OR IGNORE INTO matches(match_id, platform, patch, game_version, queue_id, game_creation, game_duration, has_irelia, fetched_at)
      VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?)`),
    upsertVersion: db.prepare(`
      INSERT INTO game_versions(platform, game_version, patch, first_seen, last_seen, games) VALUES(?, ?, ?, ?, ?, 1)
      ON CONFLICT(platform, game_version) DO UPDATE SET first_seen = MIN(first_seen, excluded.first_seen),
        last_seen = MAX(last_seen, excluded.last_seen), games = games + 1`),
    setHasIrelia: db.prepare('UPDATE matches SET has_irelia = 1 WHERE match_id = ?'),
    insertGame: db.prepare(`
      INSERT OR REPLACE INTO irelia_games(${GAME_COLUMNS.join(', ')}) VALUES(${GAME_COLUMNS.map((c) => `:${c}`).join(', ')})`),
    setSource: db.prepare('UPDATE irelia_games SET pilot_source = ? WHERE match_id = ?'),
    deleteParticipants: db.prepare(byIds
      ? 'DELETE FROM participant_rows WHERE match_ref = (SELECT id FROM match_ids WHERE match_id = ?)'
      : 'DELETE FROM participants WHERE match_id = ?'),
    insertParticipant: db.prepare(byIds
      ? `INSERT OR REPLACE INTO participant_rows(match_ref, participant_id, puuid_ref, ${PARTICIPANT_VALUE_COLUMNS.join(', ')})
         VALUES(:match_ref, :participant_id, :puuid_ref, ${PARTICIPANT_VALUE_COLUMNS.map((c) => `:${c}`).join(', ')})`
      : `INSERT OR REPLACE INTO participants(${PARTICIPANT_COLUMNS.join(', ')}) VALUES(${PARTICIPANT_COLUMNS.map((c) => `:${c}`).join(', ')})`),
    restoreParticipant: byIds ? db.prepare(`INSERT OR IGNORE INTO participant_rows(match_ref, participant_id, puuid_ref, ${PARTICIPANT_VALUE_COLUMNS.join(', ')})
         VALUES(:match_ref, :participant_id, :puuid_ref, ${PARTICIPANT_VALUE_COLUMNS.map((c) => `:${c}`).join(', ')})`) : null,
    matchRef: byIds ? db.prepare('INSERT INTO match_ids(match_id) VALUES(?) ON CONFLICT(match_id) DO UPDATE SET match_id = excluded.match_id RETURNING id') : null,
    puuidRef: byIds ? db.prepare('INSERT INTO puuid_ids(puuid) VALUES(?) ON CONFLICT(puuid) DO UPDATE SET puuid = excluded.puuid RETURNING id') : null,
    insertTimeline: db.prepare("INSERT OR REPLACE INTO timelines(match_id, frame_interval, data, data_z) VALUES(?, ?, '', ?)"),
    // The background conversion (server/convert.ts): the plain timelines after a rowid, a batch at a time.
    plainTimelines: db.prepare("SELECT rowid AS id, match_id, data FROM timelines WHERE rowid > ? AND data_z IS NULL ORDER BY rowid LIMIT ?"),
    timelinesAfter: db.prepare('SELECT MAX(rowid) AS last FROM (SELECT rowid FROM timelines WHERE rowid > ? ORDER BY rowid LIMIT ?)'),
    compressTimeline: db.prepare("UPDATE timelines SET data = '', data_z = ? WHERE rowid = ? AND data_z IS NULL AND data = ?"),
    maxTimelineRow: db.prepare('SELECT COALESCE(MAX(rowid), 0) AS n FROM timelines'),
    anyPlainAfter: db.prepare('SELECT 1 AS x FROM timelines WHERE rowid > ? AND data_z IS NULL LIMIT 1'),
    setQuality: db.prepare('UPDATE matches SET remake = ?, end_result = ?, detail_version = ? WHERE match_id = ?'),
    setLeaver: db.prepare('UPDATE matches SET leaver = ? WHERE match_id = ?'),
    setOpponents: db.prepare(`UPDATE irelia_games SET participant_id = ?, position_opponent_id = ?, timeline_opponent_id = ?,
      opponent_champion_id = ? WHERE match_id = ?`),
    fillPilotMeta: db.prepare(`UPDATE irelia_games SET pilot_tier = ?, pilot_lp = ?, pilot_mastery = ?, pilot_last_play = ?, pilot_top1 = ?
      WHERE match_id = ? AND pilot_tier IS NULL`),
    pendingDetail: db.prepare(`SELECT m.match_id, m.platform, m.patch, m.has_irelia, g.puuid AS game_puuid
      FROM matches m LEFT JOIN irelia_games g USING(match_id) WHERE m.detail_version < ? ORDER BY m.match_id LIMIT ?`),
    countPendingDetail: db.prepare('SELECT COUNT(*) AS n FROM matches WHERE detail_version < ?'),
    markDetail: db.prepare('UPDATE matches SET detail_version = ? WHERE match_id = ?'),
    resetDetail: db.prepare('UPDATE matches SET detail_version = 0'),
    gamesForPatches: db.prepare(`SELECT g.*, m.remake, m.leaver, m.end_result, m.game_version FROM irelia_games g JOIN matches m USING(match_id)
      WHERE g.patch IN (SELECT value FROM json_each(?)) AND ${SOURCE_FILTER}`),
    counts: db.prepare(`SELECT
      (SELECT COUNT(*) FROM players) AS players,
      (SELECT COUNT(*) FROM players WHERE is_otp = 1) AS otp,
      (SELECT COUNT(*) FROM players WHERE pilot_class = 'main') AS mains,
      (SELECT COUNT(*) FROM matches) AS matches,
      (SELECT COUNT(*) FROM irelia_games) AS games,
      (SELECT COUNT(*) FROM irelia_games WHERE patch = ?) AS current`),
    sourceCounts: db.prepare(`SELECT COALESCE(pilot_source, 'otp') AS source, COUNT(*) AS n FROM irelia_games GROUP BY 1`),
    patchBreakdown: db.prepare('SELECT patch, COUNT(*) AS games FROM irelia_games g WHERE ' + SOURCE_FILTER + ' GROUP BY patch ORDER BY patch DESC'),
    laneBreakdown: db.prepare('SELECT patch, position, COUNT(*) AS games FROM irelia_games g WHERE ' + SOURCE_FILTER + ' GROUP BY patch, position'),
    opponentCounts: db.prepare(`SELECT opponent_champion_id AS id, COUNT(*) AS games FROM irelia_games g
      WHERE patch IN (SELECT value FROM json_each(?)) AND opponent_champion_id IS NOT NULL AND ${SOURCE_FILTER} GROUP BY opponent_champion_id`),
    opponentCountsIn: db.prepare(`SELECT opponent_champion_id AS id, COUNT(*) AS games FROM irelia_games g
      WHERE patch IN (SELECT value FROM json_each(?)) AND position = ? AND opponent_champion_id IS NOT NULL AND ${SOURCE_FILTER}
      GROUP BY opponent_champion_id`),
    versions: db.prepare('SELECT platform, game_version, patch, first_seen, last_seen, games FROM game_versions'),
    rebuildVersions: db.prepare(`INSERT INTO game_versions(platform, game_version, patch, first_seen, last_seen, games)
      SELECT platform, game_version, patch, MIN(game_creation), MAX(game_creation), COUNT(*) FROM matches GROUP BY platform, game_version
      ON CONFLICT(platform, game_version) DO UPDATE SET first_seen = MIN(first_seen, excluded.first_seen),
        last_seen = MAX(last_seen, excluded.last_seen), games = MAX(games, excluded.games)`),
    // Retention: per-champion totals of the matches about to be purged (remakes excluded), added to what is kept.
    keepChampionStats: db.prepare(keepChampionStatsSql('m.patch NOT IN (SELECT value FROM json_each(?))')),
    // ---- v5: the archive ----
    // The next matches to archive: not on a kept patch.
    archivable: db.prepare('SELECT match_id, patch FROM matches WHERE patch NOT IN (SELECT value FROM json_each(?)) LIMIT ?'),
    anyArchivable: db.prepare('SELECT 1 AS x FROM matches WHERE patch NOT IN (SELECT value FROM json_each(?)) LIMIT 1'),
    staleCount: db.prepare('SELECT COUNT(*) AS n FROM matches WHERE patch NOT IN (SELECT value FROM json_each(?))'),
    keepChampionStatsOf: db.prepare(keepChampionStatsSql('m.match_id IN (SELECT value FROM json_each(?))')),
    // A restored match's totals come back out of champion_patch_stats: they are in its participants again.
    dropChampionStatsOf: db.prepare(`UPDATE champion_patch_stats AS s SET ${TOTAL_COLUMNS.map((c) => `${c} = s.${c} - x.${c}`).join(', ')}
      FROM (${championTotalsOf('m.match_id IN (SELECT value FROM json_each(?))')}) AS x
      WHERE s.patch = x.patch AND s.champion_id = x.champion_id AND s.position = x.position`),
    dropEmptyChampionStats: db.prepare('DELETE FROM champion_patch_stats WHERE games <= 0'),
    // A timeline goes into the archive as JSON (its data), compressed or not here.
    archiveRows: Object.fromEntries(ARCHIVED_TABLES.map((t) => [t, db.prepare(t === 'timelines'
      ? 'SELECT match_id, frame_interval, timeline_json(data, data_z) AS data FROM timelines WHERE match_id = ?'
      : `SELECT * FROM ${t} WHERE match_id = ?`)])),
    insertArchive: db.prepare('INSERT OR REPLACE INTO archive(match_id, patch, archived_at, data) VALUES(?, ?, ?, ?)'),
    deleteMatch: db.prepare('DELETE FROM matches WHERE match_id = ?'),
    nextArchived: db.prepare('SELECT match_id, data FROM archive WHERE patch IN (SELECT value FROM json_each(?)) OR ? LIMIT ?'),
    archivedData: db.prepare('SELECT data FROM archive WHERE match_id = ?'),
    deleteArchived: db.prepare('DELETE FROM archive WHERE match_id = ?'),
    matchPatches: db.prepare('SELECT match_id, patch FROM matches UNION ALL SELECT match_id, patch FROM archive'),
    timelinesOf: db.prepare('SELECT match_id, timeline_json(data, data_z) AS data FROM timelines WHERE match_id IN (SELECT value FROM json_each(?))'),
    archiveSummary: db.prepare('SELECT patch, COUNT(*) AS games, SUM(LENGTH(data)) AS bytes FROM archive GROUP BY patch ORDER BY patch'),
    purgeStale: db.prepare('DELETE FROM matches WHERE patch NOT IN (SELECT value FROM json_each(?))'),
    purgeQueue: db.prepare('DELETE FROM match_queue WHERE match_id NOT IN (SELECT match_id FROM matches) AND kind = ?'),
    // Queued twice: the higher score stays, and the user's own games take the entry over (they keep the previous
    // patch and must never be dropped by another pilot's older-patch cut).
    enqueue: db.prepare(`INSERT INTO match_queue(match_id, platform, kind, puuid, source, score, added_at) VALUES(?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT(match_id) DO UPDATE SET score = MAX(score, excluded.score),
        source = CASE WHEN excluded.source = 'me' THEN 'me' ELSE source END,
        puuid = CASE WHEN excluded.source = 'me' THEN excluded.puuid ELSE puuid END`),
    nextQueuedBatch: db.prepare(`SELECT * FROM match_queue WHERE platform IN (SELECT value FROM json_each(?))
      ORDER BY score DESC, match_id DESC LIMIT ?`),
    // Mastery work for a platform host, most valuable first: 0 players seen playing Irelia (harvested), 1 pilots due a
    // recheck (activity), 2 ladder players never checked, or checked only before v3 (top-1 mastery, which can't tell
    // a main: irelia_points is NULL), 3 everyone else due a recheck.
    masteryWork: db.prepare(`SELECT * FROM (SELECT *, CASE
        WHEN is_otp IS NULL AND found_by <> 'ladder' THEN 0
        WHEN pilot_class IN ('otp', 'main') AND (last_checked IS NULL OR last_checked < :pilotBefore) THEN 1
        WHEN is_otp IS NULL OR irelia_points IS NULL THEN 2
        WHEN last_checked < :otherBefore THEN 3
      END AS prio FROM players WHERE platform = :platform)
      WHERE prio BETWEEN :lo AND :hi ORDER BY prio, league_points DESC LIMIT :n`),
    nextQueued: db.prepare('SELECT * FROM match_queue WHERE platform IN (SELECT value FROM json_each(?)) ORDER BY score DESC, match_id DESC LIMIT 1'),
    queueSize: db.prepare('SELECT platform, kind, COUNT(*) AS n FROM match_queue GROUP BY platform, kind'),
    dequeue: db.prepare('DELETE FROM match_queue WHERE match_id = ?'),
    // Match ids grow over time on a platform: once a pilot's history reaches an older patch, older queued ids go too.
    dropOlderQueued: db.prepare(`DELETE FROM match_queue WHERE puuid = ? AND kind = 'match' AND platform = ?
      AND CAST(substr(match_id, instr(match_id, '_') + 1) AS INTEGER) < ?`),
    tableRows: db.prepare(`SELECT 'players' AS name, COUNT(*) AS n FROM players UNION ALL SELECT 'matches', COUNT(*) FROM matches
      UNION ALL SELECT 'irelia_games', COUNT(*) FROM irelia_games UNION ALL SELECT 'participants', COUNT(*) FROM participants
      UNION ALL SELECT 'timelines', COUNT(*) FROM timelines UNION ALL SELECT 'game_versions', COUNT(*) FROM game_versions
      UNION ALL SELECT 'champion_patch_stats', COUNT(*) FROM champion_patch_stats UNION ALL SELECT 'match_queue', COUNT(*) FROM match_queue
      UNION ALL SELECT 'archive', COUNT(*) FROM archive`),
    vacuumInto: db.prepare('VACUUM INTO ?'),
    // ---- v4: draft-aware model (server/model/) ----
    championTotals: db.prepare(`SELECT p.champion_id, p.position, COUNT(*) AS games, SUM(m.game_duration) AS seconds,
        SUM(p.dmg_physical) AS dmg_physical, SUM(p.dmg_magic) AS dmg_magic, SUM(p.dmg_true) AS dmg_true, SUM(p.dmg_taken) AS dmg_taken,
        SUM(p.dmg_mitigated) AS dmg_mitigated, SUM(p.heal) AS heal, SUM(p.heal_allies) AS heal_allies,
        SUM(p.shield_allies) AS shield_allies, SUM(p.cc_time) AS cc_time
      FROM participants p JOIN matches m USING(match_id)
      WHERE m.patch IN (SELECT value FROM json_each(?)) AND COALESCE(m.remake, 0) = 0 AND COALESCE(m.leaver, 0) = 0
        AND COALESCE(m.end_result, 'GameComplete') = 'GameComplete'
      GROUP BY p.champion_id, p.position`),
    olderChampionTotals: db.prepare(`SELECT champion_id, position, games, seconds, dmg_physical, dmg_magic, dmg_true, dmg_taken,
        dmg_mitigated, heal, heal_allies, shield_allies, cc_time
      FROM champion_patch_stats WHERE patch NOT IN (SELECT value FROM json_each(?))`),
    modelGames: db.prepare(`SELECT g.*, m.platform, m.remake, m.leaver, m.end_result, m.game_version, timeline_json(t.data, t.data_z) AS timeline
      FROM irelia_games g JOIN matches m USING(match_id) LEFT JOIN timelines t USING(match_id)
      WHERE g.patch IN (SELECT value FROM json_each(?)) AND ${SOURCE_FILTER}`),
    // The same rows with each timeline as stored (0.15.0): a build on helper threads decodes them there (prepareTeam.ts).
    // Only the select list differs from modelGames, so the rows come in the same order.
    modelGamesRaw: db.prepare(`SELECT g.*, m.platform, m.remake, m.leaver, m.end_result, m.game_version, t.data AS timeline, t.data_z AS timeline_z
      FROM irelia_games g JOIN matches m USING(match_id) LEFT JOIN timelines t USING(match_id)
      WHERE g.patch IN (SELECT value FROM json_each(?)) AND ${SOURCE_FILTER}`),
    modelGameCount: db.prepare(`SELECT COUNT(*) AS n FROM irelia_games g WHERE g.patch IN (SELECT value FROM json_each(?)) AND ${SOURCE_FILTER}`),
    modelParticipants: db.prepare(`SELECT p.match_id, p.participant_id, p.champion_id, p.team_id, p.position
      FROM participants p JOIN irelia_games g USING(match_id)
      WHERE g.patch IN (SELECT value FROM json_each(?)) AND ${SOURCE_FILTER}`),
    dataVersion: db.prepare(`SELECT (SELECT COUNT(*) FROM irelia_games) AS games, (SELECT COALESCE(MAX(created_at), 0) FROM irelia_games) AS last,
      (SELECT COUNT(*) FROM timelines) AS timelines, (SELECT COUNT(*) FROM matches WHERE detail_version > 0) AS captured`),
    gameMark: db.prepare('SELECT COALESCE(MAX(rowid), 0) AS row, COUNT(*) AS games FROM irelia_games'),
    ownGameCount: db.prepare("SELECT COUNT(*) AS n FROM irelia_games WHERE pilot_source = 'me'"),
    gamesAfterRow: db.prepare('SELECT COUNT(*) AS n FROM irelia_games WHERE rowid > ?'),
    logRec: db.prepare(`INSERT INTO rec_log(session, phase, created_at, updated_at, lane, opponent_id, mode, shown) VALUES(?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT(session) DO UPDATE SET updated_at = excluded.updated_at, lane = excluded.lane, opponent_id = excluded.opponent_id,
        mode = excluded.mode, shown = excluded.shown`),
    getRec: db.prepare('SELECT * FROM rec_log WHERE session = ?'),
    unlinkedRecs: db.prepare('SELECT * FROM rec_log WHERE match_id IS NULL AND created_at < ? ORDER BY created_at'),
    linkRec: db.prepare('UPDATE rec_log SET match_id = ?, bought = ?, win = ? WHERE id = ?'),
    recsFor: db.prepare('SELECT * FROM rec_log WHERE match_id = ? ORDER BY phase'),
    recLog: db.prepare('SELECT * FROM rec_log ORDER BY created_at DESC LIMIT ?'),
    participantsOf: db.prepare('SELECT match_id, participant_id, champion_id, team_id, position FROM participants WHERE match_id = ?'),
    ownGames: db.prepare(`SELECT g.*, m.platform, m.remake, m.leaver, m.end_result, m.game_version, timeline_json(t.data, t.data_z) AS timeline
      FROM irelia_games g JOIN matches m USING(match_id) LEFT JOIN timelines t USING(match_id)
      WHERE g.pilot_source = 'me' ORDER BY g.created_at DESC LIMIT ?`),
  };

  /**
   * Runs `fn` in one write transaction. `fn` must be synchronous: nothing else may write between BEGIN and COMMIT.
   * It takes the write lock first (BEGIN IMMEDIATE, waiting busy_timeout for it). A deferred transaction that reads
   * before it writes fails at once with "database is locked" when another thread writes meanwhile: SQLite can't wait
   * for the lock then, since the snapshot it read is already stale.
   */
  const tx = <T>(fn: () => T): T => {
    db.exec('BEGIN IMMEDIATE');
    try { const r = fn(); db.exec('COMMIT'); return r; } catch (err) { db.exec('ROLLBACK'); throw err; }
  };

  /** Inserts a row restored from the archive: only the table's own columns (a column added since gets its default). */
  const columnsOf = new Map<string, Set<string>>();
  const restoreStatements = new Map<string, ReturnType<typeof db.prepare>>();
  const restoreRow = (table: (typeof ARCHIVED_TABLES)[number], row: Record<string, unknown>) => {
    let have = columnsOf.get(table);
    if (!have) { have = new Set((db.prepare(`PRAGMA table_info(${table})`).all() as { name: string }[]).map((c) => c.name)); columnsOf.set(table, have); }
    const cols = Object.keys(row).filter((c) => have.has(c));
    const key = `${table}:${cols.join(',')}`;
    let st = restoreStatements.get(key);
    if (!st) { st = db.prepare(`INSERT OR IGNORE INTO ${table}(${cols.join(', ')}) VALUES(${cols.map(() => '?').join(', ')})`); restoreStatements.set(key, st); }
    st.run(...cols.map((c) => row[c] as string | number | null));
  };

  /** A participants row as the insert takes it: by ids when participants is the view (the match's and PUUID's id made if new). */
  const participantValues = (r: Record<string, unknown>) => {
    if (!byIds) return Object.fromEntries(PARTICIPANT_COLUMNS.map((c) => [c, r[c] ?? null])) as Record<string, string | number | null>;
    const { match_id, puuid, ...rest } = r;
    return {
      ...Object.fromEntries(['participant_id', ...PARTICIPANT_VALUE_COLUMNS].map((c) => [c, rest[c] ?? null])),
      match_ref: (q.matchRef!.get(match_id as string) as { id: number }).id,
      puuid_ref: (q.puuidRef!.get(puuid as string) as { id: number }).id,
    } as Record<string, string | number | null>;
  };

  const insertMatch = (m: MatchRow) => {
    const fresh = Number(q.insertMatch.run(m.id, m.platform, m.patch, m.version, m.queue, m.creation, m.duration, m.hasIrelia ? 1 : 0, Date.now()).changes) > 0;
    if (fresh) q.upsertVersion.run(m.platform, m.version, m.patch, m.creation, m.creation);
    return fresh;
  };

  return {
    raw: db,
    tx,
    getMeta: (key: string) => (q.getMeta.get(key) as { value: string } | undefined)?.value ?? null,
    setMeta: (key: string, value: string) => void q.setMeta.run(key, value),
    upsertPlayer: (puuid: string, platform: string, tier: string, lp: number) =>
      void q.upsertPlayer.run(puuid, platform, tier, lp, Date.now()),
    /** A player found outside the ladder (a harvested Irelia, the user's account): added unchecked, never overwritten. */
    addCandidate: (puuid: string, platform: string, foundBy: 'harvest' | 'me') =>
      Number(q.addCandidate.run(puuid, platform, Date.now(), foundBy).changes) > 0,
    setRank: (puuid: string, tier: string, lp: number) => void q.setRank.run(tier, lp, Date.now(), Date.now(), puuid),
    player: (puuid: string) => q.getPlayer.get(puuid) as PlayerRow | undefined,
    setPilot: (puuid: string, u: PilotUpdate) => void q.setPilot.run(
      u.pilotClass === 'otp' ? 1 : 0, u.ireliaRank === 1 ? u.ireliaPoints : 0, Date.now(), u.pilotClass, u.ireliaPoints,
      u.ireliaLastPlay, u.ireliaRank, puuid),
    /** Pilots on `platform` whose history is due for a scan: of `classes`, scanned before `olderThan`, active since `activeSince`. */
    /**
     * Pilots on `platform` checked since `since` whose last Irelia game is still before it: due for a mastery check
     * again (0.21.2: 0.21.1's patch recheck read mastery answers cached before the patch). Returns how many.
     */
    forgetActivityChecks: (platform: string, since: number) => Number(q.forgetActivityChecks.run(platform, since, since).changes),
    duePilots: (platform: string, classes: string[], olderThan: number, activeSince: number, limit: number) =>
      q.duePilots.all(platform, JSON.stringify(classes), olderThan, activeSince, limit) as unknown as PlayerRow[],
    /** Irelia mastery facts read from a cached mastery response (backfill: OTPs checked before v3). */
    setIreliaActivity: (puuid: string, points: number, lastPlay: number | null, rank: number | null) =>
      void q.setActivity.run(points, lastPlay, rank, puuid),
    /** OTPs whose Irelia activity is unknown (checked before v3 read lastPlayTime). */
    playersMissingActivity: () => q.missingActivity.all() as unknown as PlayerRow[],
    /** Tags the user's own stored Irelia games 'me' so they never feed the high-elo builds. */
    tagOwnGames: (puuid: string) => Number(q.tagOwn.run(puuid).changes),
    countHistory: (puuid: string, matches: number, irelia: number) => void q.countHistory.run(matches, irelia, puuid),
    unrankedCandidates: (platform: string, limit: number) => q.unrankedCandidates.all(platform, limit) as unknown as PlayerRow[],
    setScanned: (puuid: string) => void q.setScanned.run(Date.now(), puuid),
    hasMatch: (id: string) => q.hasMatch.get(id) !== undefined,
    /** The stored Irelia game's source, or undefined when the match holds none. */
    gameSource: (id: string) => (q.hasGame.get(id) as { pilot_source: PilotSource | null } | undefined)?.pilot_source,
    /** Records a match; returns false when it was already stored. Also keeps game_versions (patch per region, hotfix builds). */
    insertMatch,
    setHasIrelia: (id: string) => void q.setHasIrelia.run(id),
    insertGame: (row: IreliaGameRow) => void q.insertGame.run(
      Object.fromEntries(GAME_COLUMNS.map((c) => [c, (row as unknown as Record<string, string | number | null | undefined>)[c] ?? null])) as Record<string, string | number | null>),
    setGameSource: (id: string, source: PilotSource) => void q.setSource.run(source, id),
    /** Replaces a match's participants (idempotent). */
    insertParticipants: (matchId: string, rows: ParticipantRow[]) => {
      q.deleteParticipants.run(matchId);
      for (const r of rows) q.insertParticipant.run(participantValues(r as unknown as Record<string, unknown>));
    },
    /** Stores a game's timeline capture (JSON), compressed (0.14.0). */
    insertTimeline: (matchId: string, interval: number, data: string) => void q.insertTimeline.run(matchId, interval, packTimeline(data)),
    setQuality: (matchId: string, remake: boolean, endResult: string | null, detailVersion: number) =>
      void q.setQuality.run(remake ? 1 : 0, endResult, detailVersion, matchId),
    setLeaver: (matchId: string, leaver: boolean) => void q.setLeaver.run(leaver ? 1 : 0, matchId),
    setOpponents: (matchId: string, o: { participantId: number; position: number | null; timeline: number | null; effective: number | null }) =>
      void q.setOpponents.run(o.participantId, o.position, o.timeline, o.effective, matchId),
    /** Pilot metadata for a game stored before v3, from the player's row (their current tier, not the one at scan time). */
    fillPilotMeta: (matchId: string, p: PlayerRow) => void q.fillPilotMeta.run(
      p.tier, p.league_points, p.irelia_points ?? p.irelia_mastery ?? null, p.irelia_last_play ?? null,
      p.irelia_rank === 1 || p.is_otp === 1 ? 1 : p.irelia_rank ? 0 : null, matchId),
    /** Matches whose capture is older than `version` (the backfill's work list), in a stable order. */
    pendingDetail: (version: number, limit: number) =>
      q.pendingDetail.all(version, limit) as { match_id: string; platform: string; patch: string; has_irelia: number; game_puuid: string | null }[],
    countPendingDetail: (version: number) => (q.countPendingDetail.get(version) as { n: number }).n,
    markDetail: (matchId: string, version: number) => void q.markDetail.run(version, matchId),
    /** Makes the backfill redo every match (POST /api/db/backfill {force:true}). */
    resetDetail: () => Number(q.resetDetail.run().changes),
    // Kept for a while (the model thread's lane builds, 0.19.0): as plain copies, about a third of node:sqlite's rows.
    gamesForPatches: (patches: string[], sources: readonly string[] = DEFAULT_BUILD_SOURCES) =>
      compactRows(q.gamesForPatches.all(JSON.stringify(patches), json(sources))) as unknown as IreliaGameRow[],
    counts: (currentPatch: string) =>
      q.counts.get(currentPatch) as { players: number; otp: number; mains: number; matches: number; games: number; current: number },
    /** Stored Irelia games per source: { otp: 900, harvested: 30, me: 12 }. */
    sourceCounts: () => Object.fromEntries((q.sourceCounts.all() as { source: string; n: number }[]).map((r) => [r.source, r.n])) as Partial<Record<PilotSource, number>>,
    /** Build-source Irelia games per patch, newest first, split by position: { patch, games, lanes: { TOP: 84, MIDDLE: 31 } }. */
    patchBreakdown: (sources: readonly string[] = DEFAULT_BUILD_SOURCES) => {
      const lanes = q.laneBreakdown.all(json(sources)) as { patch: string; position: string; games: number }[];
      return (q.patchBreakdown.all(json(sources)) as { patch: string; games: number }[]).map((p) => ({
        ...p,
        lanes: Object.fromEntries(lanes.filter((l) => l.patch === p.patch).map((l) => [l.position, l.games])) as Record<string, number>,
      }));
    },
    /** Opponents Irelia faced, optionally only in one position (match-v5 teamPosition, e.g. "MIDDLE"). */
    opponentCounts: (patches: string[], position?: string, sources: readonly string[] = DEFAULT_BUILD_SOURCES) => (position
      ? q.opponentCountsIn.all(JSON.stringify(patches), position, json(sources))
      : q.opponentCounts.all(JSON.stringify(patches), json(sources))) as { id: number; games: number }[],
    /** Every game build seen, per platform (server/patches.ts reads the patch per region and hotfix builds from it). */
    gameVersions: () => q.versions.all() as { platform: string; game_version: string; patch: string; first_seen: number; last_seen: number; games: number }[],
    /** Rebuilds game_versions from the stored matches (backfill; idempotent). */
    rebuildGameVersions: () => void q.rebuildVersions.run(),
    /**
     * Deletes matches (and cascaded Irelia games, participants and timelines) not on one of the given patches. Their
     * per-champion totals are kept in champion_patch_stats first, in the same transaction.
     */
    purgeStale: (keepPatches: string[]) => tx(() => {
      q.keepChampionStats.run(JSON.stringify(keepPatches));
      const removed = Number(q.purgeStale.run(JSON.stringify(keepPatches)).changes);
      q.purgeQueue.run('timeline');
      return removed;
    }),
    /**
     * Archives up to `limit` matches not on one of `keepPatches`: each stored whole (its rows in every table),
     * compressed, then deleted from the working tables, its per-champion totals kept as purgeStale keeps them. One
     * transaction per call; returns how many it moved (0: none left).
     */
    archiveBatch: (keepPatches: string[], limit = 200) => tx(() => {
      const batch = q.archivable.all(JSON.stringify(keepPatches), limit) as { match_id: string; patch: string }[];
      if (!batch.length) return 0;
      q.keepChampionStatsOf.run(JSON.stringify(batch.map((m) => m.match_id)));
      const now = Date.now();
      for (const { match_id, patch } of batch) {
        const rows = Object.fromEntries(ARCHIVED_TABLES.map((t) => [t, q.archiveRows[t].all(match_id)]));
        q.insertArchive.run(match_id, patch, now, zlib.gzipSync(JSON.stringify(rows)));
        q.deleteMatch.run(match_id);
      }
      q.purgeQueue.run('timeline');
      return batch.length;
    }),
    /**
     * Puts up to `limit` archived matches (of `patches`; null: any) back into the working tables, as they were, and
     * takes their per-champion totals back out of the kept ones. A match stored again since keeps its new rows.
     */
    restoreBatch: (patches: string[] | null, limit = 200) => {
      // Read, and each timeline compressed again (as every timeline is stored now), before the write lock: zstd takes a
      // few milliseconds a timeline, and fetching's writes wait for the lock meanwhile (0.14.0).
      const prepare = (data: Uint8Array) => {
        const rows = JSON.parse(zlib.gunzipSync(data).toString('utf8')) as Record<string, Record<string, unknown>[]>;
        rows.timelines = (rows.timelines ?? []).map((row) => (typeof row.data === 'string' ? { ...row, data: '', data_z: packTimeline(row.data) } : row));
        return rows;
      };
      const batch = (q.nextArchived.all(JSON.stringify(patches ?? []), patches ? 0 : 1, limit) as { match_id: string; data: Uint8Array }[])
        .map((b) => ({ ...b, rows: prepare(b.data) }));
      if (!batch.length) return 0;
      return tx(() => {
        const restored: string[] = [];
        for (const { match_id, data, rows: read } of batch) {
          // Changed since it was read (nothing else restores or archives meanwhile, but if it did): read again here.
          const now = q.archivedData.get(match_id) as { data: Uint8Array } | undefined;
          if (!now) continue;
          const rows = Buffer.from(now.data).equals(data) ? read : prepare(now.data);
          for (const t of ARCHIVED_TABLES) {
            for (const row of rows[t] ?? []) {
              // Participants by ids where they are.
              if (t === 'participants' && byIds) q.restoreParticipant!.run(participantValues(row));
              else restoreRow(t, row);
            }
          }
          // INSERT OR IGNORE skips a row it can't insert: the archived copy goes only once the match is back.
          if (q.hasMatch.get(match_id) === undefined) throw new Error(`Could not restore ${match_id}: nothing was changed`);
          q.deleteArchived.run(match_id);
          restored.push(match_id);
        }
        q.dropChampionStatsOf.run(JSON.stringify(restored));
        q.dropEmptyChampionStats.run();
        return restored.length;
      });
    },
    // ---- 0.18.0: copying matches between databases (backing up and importing the pool, server/transfer.ts) ----
    /** One stored match whole, in the archive's shape: its rows of every table, the timeline as JSON. */
    matchRecord: (matchId: string): MatchRecord =>
      Object.fromEntries(ARCHIVED_TABLES.map((t) => [t, q.archiveRows[t].all(matchId)])) as unknown as MatchRecord,
    /** Every stored Irelia game's match id, oldest first. */
    ireliaMatchIds: () => (db.prepare('SELECT match_id FROM irelia_games ORDER BY match_id').all() as { match_id: string }[]).map((r) => r.match_id),
    /**
     * Stores matches copied from another database, in one transaction; a match stored here already is skipped (this
     * database's rows win). Timelines are compressed with this database's dictionary. `source` replaces the Irelia
     * games' source (Copy into pool). Returns how many were added.
     */
    importRecords: (records: MatchRecord[], o: { source?: PilotSource } = {}) => {
      // Timelines compressed before the write lock, as restoreBatch does.
      const ready = records.map((r) => ({
        ...r, timelines: (r.timelines ?? []).map((row) => (typeof row.data === 'string' && row.data ? { ...row, data: '', data_z: packTimeline(row.data) } : row)),
      }));
      return tx(() => {
        let added = 0;
        for (const rec of ready) {
          const m = rec.matches?.[0] as Record<string, unknown> | undefined;
          if (!m || typeof m.match_id !== 'string' || q.hasMatch.get(m.match_id) !== undefined) continue;
          for (const t of ARCHIVED_TABLES) {
            for (const row of (rec[t] ?? []) as Record<string, unknown>[]) {
              if (t === 'participants' && byIds) q.restoreParticipant!.run(participantValues(row));
              else if (t === 'irelia_games' && o.source) restoreRow(t, { ...row, pilot_source: o.source });
              else restoreRow(t, row);
            }
          }
          q.upsertVersion.run(m.platform as string, m.game_version as string, m.patch as string, m.game_creation as number, m.game_creation as number);
          added++;
        }
        return added;
      });
    },
    /**
     * For an export copy only (0.18.0): other players' PUUIDs in the matches are blanked. `keep` are kept (the pool's
     * pilots and the Irelia players of its games). Champions, items and stats stay: the builds need nothing else. The pool's
     * archived matches are stripped the same way, one at a time.
     */
    stripOtherPlayers: (keep: string[]) => tx(() => {
      const k = JSON.stringify(keep);
      if (byIds) {
        db.prepare("INSERT OR IGNORE INTO puuid_ids(puuid) VALUES('')").run();
        db.prepare(`UPDATE participant_rows SET puuid_ref = (SELECT id FROM puuid_ids WHERE puuid = '')
          WHERE puuid_ref NOT IN (SELECT id FROM puuid_ids WHERE puuid IN (SELECT value FROM json_each(?)))`).run(k);
        db.prepare("DELETE FROM puuid_ids WHERE puuid <> '' AND id NOT IN (SELECT puuid_ref FROM participant_rows)").run();
      } else {
        db.prepare("UPDATE participants SET puuid = '' WHERE puuid NOT IN (SELECT value FROM json_each(?))").run(k);
      }
      const keepSet = new Set(keep);
      const rows = db.prepare('SELECT match_id, data FROM archive').all() as { match_id: string; data: Uint8Array }[];
      const update = db.prepare('UPDATE archive SET data = ? WHERE match_id = ?');
      for (const r of rows) {
        const rec = JSON.parse(zlib.gunzipSync(r.data).toString('utf8')) as Record<string, Record<string, unknown>[]>;
        for (const p of rec.participants ?? []) if (!keepSet.has(p.puuid as string)) p.puuid = '';
        update.run(zlib.gzipSync(JSON.stringify(rec)), r.match_id);
      }
      // Work in progress names players too, and the next fetch rebuilds it.
      db.prepare('DELETE FROM match_queue').run();
    }),
    /** The stored timelines (JSON) of these matches, by match id. */
    timelinesOf: (matchIds: string[]) => new Map((q.timelinesOf.all(JSON.stringify(matchIds)) as { match_id: string; data: string }[]).map((r) => [r.match_id, r.data])),
    /** Whether a match not on one of `keepPatches` is stored (something to archive). */
    hasArchivable: (keepPatches: string[]) => q.anyArchivable.get(JSON.stringify(keepPatches)) !== undefined,
    /** How many stored matches are of other patches than these (what archiving or deleting old patches goes through). */
    staleCount: (keepPatches: string[]) => (q.staleCount.get(JSON.stringify(keepPatches)) as { n: number }).n,
    /** Every known match id's patch, stored or archived (the cache cleanup). */
    matchPatches: () => new Map((q.matchPatches.all() as { match_id: string; patch: string }[]).map((r) => [r.match_id, r.patch])),
    /** Archived matches per patch, with their compressed size. */
    archiveSummary: () => q.archiveSummary.all() as { patch: string; games: number; bytes: number }[],
    enqueue: (m: QueuedMatch) => void q.enqueue.run(m.match_id, m.platform, m.kind, m.puuid, m.source, m.score, Date.now()),
    nextQueuedBatch: (platforms: string[], limit: number) =>
      q.nextQueuedBatch.all(JSON.stringify(platforms), limit) as unknown as QueuedMatch[],
    /** Players on `platform` whose mastery should be (re)checked, priorities lo..hi (see the query), most valuable first. */
    masteryWork: (platform: string, o: { pilotBefore: number; otherBefore: number; lo: number; hi: number; limit: number }) =>
      q.masteryWork.all({ platform, pilotBefore: o.pilotBefore, otherBefore: o.otherBefore, lo: o.lo, hi: o.hi, n: o.limit }) as unknown as (PlayerRow & { prio: number })[],
    nextQueued: (platforms: string[]) => q.nextQueued.get(JSON.stringify(platforms)) as QueuedMatch | undefined,
    dequeue: (id: string) => void q.dequeue.run(id),
    dropOlderQueued: (puuid: string, platform: string, olderThanId: number) => Number(q.dropOlderQueued.run(puuid, platform, olderThanId).changes),
    /** Queued work per platform and kind: [{ platform: 'KR', kind: 'match', n: 120 }]. */
    queueSize: () => q.queueSize.all() as { platform: string; kind: string; n: number }[],
    /** Row counts per table, and bytes when SQLite's dbstat is available (Storage manager). */
    tableStats: () => {
      const rows = q.tableRows.all() as { name: string; n: number }[];
      let bytes = new Map<string, number>();
      try {
        bytes = new Map((db.prepare(`SELECT COALESCE(i.tbl_name, d.name) AS name, SUM(d.pgsize) AS b FROM dbstat d
          LEFT JOIN sqlite_schema i ON i.name = d.name WHERE d.aggregate = TRUE GROUP BY 1`).all() as { name: string; b: number }[]).map((r) => [r.name, r.b]));
      } catch { /* dbstat not compiled in: row counts only */ }
      return rows.map((r) => ({ name: r.name, rows: r.n, bytes: bytes.get(r.name) ?? null }));
    },
    /** Row counts per table, used to verify a copied database. */
    rowCounts: () => Object.fromEntries((q.tableRows.all() as { name: string; n: number }[]).map((r) => [r.name, r.n])) as Record<string, number>,
    /** Consistent, compacted copy of the live database (WAL included) at `file`, which must not exist. */
    vacuumInto: (file: string) => void q.vacuumInto.run(file),
    /**
     * Compresses up to `limit` plain timelines after rowid `after` (0.14.0, server/convert.ts): each is compressed,
     * read back and compared before its plain copy is replaced, in one transaction per batch. Returns the last rowid
     * looked at (the next call's `after`; null: none left) and how many were converted.
     */
    compressTimelines: (after: number, limit = 200) => {
      const rows = q.plainTimelines.all(after, limit) as { id: number; match_id: string; data: string }[];
      // Rows already compressed between them are passed over too.
      const last = rows.length ? rows[rows.length - 1].id : (q.timelinesAfter.get(after, limit) as { last: number | null }).last;
      if (last === null) return { next: null, converted: 0 };
      const packed = rows.map((r) => {
        const z = packTimeline(r.data);
        if (unpack(z, dicts.get).toString('utf8') !== r.data) throw new Error(`Timeline ${r.match_id} did not compress back to itself: nothing was changed`);
        return { ...r, z };
      });
      const converted = tx(() => packed.reduce((n, r) => n + Number(q.compressTimeline.run(r.z, r.id, r.data).changes), 0));
      return { next: last, converted };
    },
    /** The timelines' last rowid: how far the conversion has to go. */
    lastTimelineRow: () => (q.maxTimelineRow.get() as { n: number }).n,
    /** Whether a plain (uncompressed) timeline is left after rowid `after` (the conversion's cursor). */
    plainTimelinesAfter: (after: number) => q.anyPlainAfter.get(after) !== undefined,
    /**
     * Gives free pages back to the file system (auto_vacuum = INCREMENTAL, 0.14.0): after deletions and conversions.
     * A database made before 0.14.0 needs one full VACUUM first (Compact database); until then it does nothing.
     */
    reclaim: () => { db.exec('PRAGMA incremental_vacuum;'); db.exec('PRAGMA wal_checkpoint(PASSIVE);'); },
    /**
     * Participants by integer ids (0.14.0, Compact database runs it with every other thread stopped): when
     * participants and its index are over PARTICIPANTS_SHARE of the database, the table is rebuilt as participant_rows
     * with a view in its place, in one transaction: every row is compared both ways (EXCEPT) before the old table goes,
     * and the change is kept only if it is smaller. What it did is recorded in meta (participants_layout). This
     * connection's statements still name the old table afterwards: open the database again.
     */
    normalizeParticipants: () => {
      const record = (r: Record<string, unknown>) => { q.setMeta.run('participants_layout', JSON.stringify({ ...r, at: Date.now() })); return r; };
      if (byIds) return { done: false, why: 'already by ids' };
      let pages: (names: string[]) => number, total: number;
      try {
        const size = db.prepare('SELECT COALESCE(SUM(pgsize), 0) AS b FROM dbstat WHERE name IN (SELECT value FROM json_each(?))');
        pages = (names) => (size.get(JSON.stringify(names)) as { b: number }).b;
        total = (db.prepare('SELECT COALESCE(SUM(pgsize), 0) AS b FROM dbstat').get() as { b: number }).b;
      } catch { return { done: false, why: 'the database can\'t be measured here (no dbstat)' }; }
      const before = pages(['participants', 'idx_participants_champion']);
      const share = total ? before / total : 0;
      if (share <= PARTICIPANTS_SHARE) return record({ done: false, share, before, why: `participants are ${(share * 100).toFixed(0)} % of the database (the rebuild needs over ${PARTICIPANTS_SHARE * 100} %)` });
      return tx(() => {
        db.exec(PARTICIPANTS_BY_IDS);
        const count = (sql: string) => (db.prepare(`SELECT COUNT(*) AS n FROM (${sql})`).get() as { n: number }).n;
        const rows = count('SELECT 1 FROM participants');
        if (count('SELECT 1 FROM participant_rows') !== rows || count(`SELECT * FROM participants EXCEPT ${PARTICIPANTS_VIEW_SELECT}`) !== 0
          || count(`${PARTICIPANTS_VIEW_SELECT} EXCEPT SELECT * FROM participants`) !== 0) {
          throw new Error('Participants by ids did not read back the same rows: nothing was changed');
        }
        const after = pages(['participant_rows', 'idx_participant_rows_champion', 'match_ids', 'puuid_ids', 'sqlite_autoindex_match_ids_1', 'sqlite_autoindex_puuid_ids_1']);
        if (after >= before) {
          // Not smaller here (few PUUIDs repeat): undone, the table stays as it was.
          db.exec('DROP TABLE participant_rows; DROP TABLE match_ids; DROP TABLE puuid_ids;');
          return record({ done: false, share, before, after, why: 'by ids it would not be smaller here' });
        }
        db.exec('DROP TABLE participants;');
        db.exec(PARTICIPANTS_VIEW);
        return record({ done: true, share, before, after, rows });
      });
    },
    /** Whether free pages come back by themselves (false: the database needs Compact database once). */
    autoVacuum: () => (db.prepare('PRAGMA auto_vacuum').get() as { auto_vacuum: number }).auto_vacuum === 2,
    /** Unused pages inside the file (what Compact or reclaim would give back), in bytes. */
    freeBytes: () => {
      const n = (db.prepare('PRAGMA freelist_count').get() as { freelist_count: number }).freelist_count;
      return n * (db.prepare('PRAGMA page_size').get() as { page_size: number }).page_size;
    },
    // ---- v4: draft-aware model ----
    /** Per champion and position, summed stats of the given patches' matches (remakes, leavers and aborted games left out). */
    championTotals: (patches: string[]) => q.championTotals.all(JSON.stringify(patches)) as unknown as ChampionTotals[],
    /** The same, from the totals kept for purged patches. */
    olderChampionTotals: (exceptPatches: string[]) => q.olderChampionTotals.all(JSON.stringify(exceptPatches)) as unknown as ChampionTotals[],
    /** Irelia games with platform, quality flags and timeline (null when there is none), for the draft model. */
    modelGames: (patches: string[], sources: readonly string[]) =>
      q.modelGames.all(JSON.stringify(patches), json(sources)) as unknown as ModelGameRow[],
    /** The same rows one at a time (the model build: each raw timeline can go once its game is prepared). */
    modelGamesIter: (patches: string[], sources: readonly string[]) =>
      q.modelGames.iterate(JSON.stringify(patches), json(sources)) as unknown as Iterable<ModelGameRow>,
    /**
     * modelGamesIter's rows with each timeline as stored: `timeline` (plain JSON) or `timeline_z` (compressed with one of
     * timelineDicts), for helper threads to decode (0.15.0).
     */
    modelGamesRawIter: (patches: string[], sources: readonly string[]) =>
      q.modelGamesRaw.iterate(JSON.stringify(patches), json(sources)) as unknown as Iterable<ModelGameRow & { timeline_z: Uint8Array | null }>,
    /** The dictionaries the timelines are compressed with (codec.ts), by id. */
    timelineDicts: () => dicts.all(),
    /** How many games modelGames would read. */
    modelGameCount: (patches: string[], sources: readonly string[]) =>
      (q.modelGameCount.get(JSON.stringify(patches), json(sources)) as { n: number }).n,
    modelParticipants: (patches: string[], sources: readonly string[]) =>
      q.modelParticipants.all(JSON.stringify(patches), json(sources)) as unknown as ParticipantLite[],
    /** Changes whenever games, timelines or captures change: the draft model rebuilds then. */
    dataVersion: () => { const v = q.dataVersion.get() as Record<string, number>; return `${v.games}:${v.last}:${v.timelines}:${v.captured}`; },
    /** Your own stored games (0.15.0: a new one rebuilds the model soon, whatever else changed). */
    ownGameCount: () => (q.ownGameCount.get() as { n: number }).n,
    /** The newest stored game's row and the number of games: where an evaluation's "new games" start (server/autoEval.ts). */
    gameMark: () => q.gameMark.get() as { row: number; games: number },
    /** Games stored after that row (rows only grow, except when Compact database renumbers them). */
    gamesAfterRow: (row: number) => (q.gamesAfterRow.get(row) as { n: number }).n,
    /** Records (or updates) the build shown in one phase of a game. */
    logRecommendation: (r: { session: string; phase: 'draft' | 'live'; lane: string | null; opponentId: number | null; mode: string; shown: unknown }) => {
      const now = Date.now();
      q.logRec.run(r.session, r.phase, now, now, r.lane, r.opponentId, r.mode, JSON.stringify(r.shown));
    },
    recommendation: (session: string) => q.getRec.get(session) as RecLogRow | undefined,
    unlinkedRecommendations: (before: number) => q.unlinkedRecs.all(before) as unknown as RecLogRow[],
    linkRecommendation: (id: number, matchId: string, bought: unknown, win: boolean) => void q.linkRec.run(matchId, JSON.stringify(bought), win ? 1 : 0, id),
    recommendationsFor: (matchId: string) => q.recsFor.all(matchId) as unknown as RecLogRow[],
    recentRecommendations: (limit: number) => q.recLog.all(limit) as unknown as RecLogRow[],
    participantsOf: (matchId: string) => q.participantsOf.all(matchId) as unknown as ParticipantLite[],
    /** Your own stored Irelia games, newest first. */
    ownGames: (limit: number) => q.ownGames.all(limit) as unknown as ModelGameRow[],
    close: () => db.close(),
  };
}

export type Db = ReturnType<typeof openDb>;

/**
 * A Db whose connection can be replaced at runtime (data-folder move) without re-wiring the
 * modules holding it: `handle` forwards every call to the current connection.
 */
export function swappableDb(initial: Db) {
  let current = initial;
  const handle = new Proxy({} as Db, { get: (_t, key) => Reflect.get(current, key) });
  return {
    handle,
    /** Installs `next` and returns the previous connection for the caller to close. */
    swap(next: Db) { const prev = current; current = next; return prev; },
  };
}
export type SwappableDb = ReturnType<typeof swappableDb>;
