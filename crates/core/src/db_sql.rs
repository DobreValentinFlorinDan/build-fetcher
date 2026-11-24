//! SQL text of server/db.ts, copied verbatim (generated from the 0.22.0 source).

pub const PRAGMAS: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA journal_size_limit = 67108864;
"#;

pub const SCHEMA: &str = r#"
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

CREATE TABLE IF NOT EXISTS codec_dicts (
  id         INTEGER PRIMARY KEY,
  bytes      BLOB NOT NULL,
  made_at    INTEGER NOT NULL,
  made_from  TEXT NOT NULL               -- what the examples were (match ids)
);
"#;

pub const PARTICIPANTS_TABLE: &str = r#"
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
CREATE INDEX IF NOT EXISTS idx_participants_champion ON participants(champion_id, position);"#;

pub const PARTICIPANTS_BY_IDS: &str = r#"
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
INSERT INTO participant_rows SELECT m.id, p.participant_id, u.id, p.champion_id, p.team_id, p.position, p.win, p.champ_level, p.kills, p.deaths, p.assists, p.gold_earned, p.cs, p.items, p.dmg_physical, p.dmg_magic, p.dmg_true, p.dmg_taken, p.dmg_mitigated, p.heal, p.heal_allies, p.shield_allies, p.cc_time, p.cc_dealt, p.time_played, p.challenges
  FROM participants p JOIN match_ids m USING(match_id) JOIN puuid_ids u USING(puuid);
CREATE INDEX idx_participant_rows_champion ON participant_rows(champion_id, position);
"#;

pub const PARTICIPANTS_VIEW: &str = r#"
CREATE VIEW participants AS SELECT m.match_id AS match_id, p.participant_id AS participant_id, u.puuid AS puuid,
  p.champion_id AS champion_id, p.team_id AS team_id, p.position AS position, p.win AS win, p.champ_level AS champ_level, p.kills AS kills, p.deaths AS deaths, p.assists AS assists, p.gold_earned AS gold_earned, p.cs AS cs, p.items AS items, p.dmg_physical AS dmg_physical, p.dmg_magic AS dmg_magic, p.dmg_true AS dmg_true, p.dmg_taken AS dmg_taken, p.dmg_mitigated AS dmg_mitigated, p.heal AS heal, p.heal_allies AS heal_allies, p.shield_allies AS shield_allies, p.cc_time AS cc_time, p.cc_dealt AS cc_dealt, p.time_played AS time_played, p.challenges AS challenges
  FROM participant_rows p JOIN match_ids m ON m.id = p.match_ref LEFT JOIN puuid_ids u ON u.id = p.puuid_ref;
CREATE TRIGGER participants_of_deleted_match AFTER DELETE ON matches BEGIN
  DELETE FROM participant_rows WHERE match_ref = (SELECT id FROM match_ids WHERE match_id = old.match_id);
  DELETE FROM match_ids WHERE match_id = old.match_id;
END;
"#;

pub const MIGRATE_V3: &str = r#"
CREATE INDEX IF NOT EXISTS idx_players_pilot ON players(platform, pilot_class, irelia_last_play);
CREATE INDEX IF NOT EXISTS idx_irelia_source ON irelia_games(pilot_source, patch);
CREATE INDEX IF NOT EXISTS idx_matches_detail ON matches(detail_version);
-- Every game stored before v3 came from an OTP's own history.
UPDATE irelia_games SET pilot_source = 'otp' WHERE pilot_source IS NULL;
UPDATE irelia_games SET position_opponent_id = opponent_champion_id WHERE position_opponent_id IS NULL AND timeline_opponent_id IS NULL;
UPDATE players SET pilot_class = CASE WHEN is_otp = 1 THEN 'otp' ELSE 'none' END WHERE pilot_class IS NULL AND is_otp IS NOT NULL;
"#;

pub const PARTICIPANTS_VIEW_SELECT: &str = r#"SELECT m.match_id AS match_id, p.participant_id AS participant_id, u.puuid AS puuid,
  p.champion_id AS champion_id, p.team_id AS team_id, p.position AS position, p.win AS win, p.champ_level AS champ_level, p.kills AS kills, p.deaths AS deaths, p.assists AS assists, p.gold_earned AS gold_earned, p.cs AS cs, p.items AS items, p.dmg_physical AS dmg_physical, p.dmg_magic AS dmg_magic, p.dmg_true AS dmg_true, p.dmg_taken AS dmg_taken, p.dmg_mitigated AS dmg_mitigated, p.heal AS heal, p.heal_allies AS heal_allies, p.shield_allies AS shield_allies, p.cc_time AS cc_time, p.cc_dealt AS cc_dealt, p.time_played AS time_played, p.challenges AS challenges
  FROM participant_rows p JOIN match_ids m ON m.id = p.match_ref LEFT JOIN puuid_ids u ON u.id = p.puuid_ref"#;
