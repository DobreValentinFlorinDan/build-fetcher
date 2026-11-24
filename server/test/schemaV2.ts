// The database schema as v0.3.0 (schema v2) created it, verbatim from server/db.ts before build sourcing v2.
// The migration and audit tests start from a database made with it.
export const SCHEMA_V2 = `
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;

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

-- v2: platform ids are stored upper-case ("KR", "EUW1") to match routing.ts
UPDATE players SET platform = UPPER(platform) WHERE platform <> UPPER(platform);
UPDATE matches SET platform = UPPER(platform) WHERE platform <> UPPER(platform);
CREATE INDEX IF NOT EXISTS idx_players_platform ON players(platform, is_otp);
`;
