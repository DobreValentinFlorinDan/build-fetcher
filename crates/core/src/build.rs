//! The contract between Build Fetcher's parts. The store (crates/store) produces match records and tier B counts;
//! the learner (crates/learn) turns them into recommendations; the server (crates/server) shows them. Each part
//! depends on these types and traits only, never on another part's internals, so each can be tested with fakes.
//!
//! JSON shapes (serde, camelCase) are what the server sends to the dashboard and the overlay.

use serde::{Deserialize, Serialize};
use std::fmt;

// ------------------------------------------------------------------------------------------------ identities

/// A lane role, as Riot's `teamPosition` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Role {
    Top,
    Jungle,
    Middle,
    Bottom,
    Utility,
}

impl Role {
    pub const ALL: [Role; 5] = [Role::Top, Role::Jungle, Role::Middle, Role::Bottom, Role::Utility];

    /// Riot's `teamPosition` / `individualPosition` ("TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY"); the LCU's
    /// `assignedPosition` ("top", "jungle", "middle", "bottom", "utility") is accepted too. Anything else is None.
    pub fn from_riot(s: &str) -> Option<Role> {
        match s.to_ascii_uppercase().as_str() {
            "TOP" => Some(Role::Top),
            "JUNGLE" => Some(Role::Jungle),
            "MIDDLE" | "MID" => Some(Role::Middle),
            "BOTTOM" | "BOT" | "ADC" => Some(Role::Bottom),
            "UTILITY" | "SUPPORT" => Some(Role::Utility),
            _ => None,
        }
    }

    pub fn as_riot(self) -> &'static str {
        match self {
            Role::Top => "TOP",
            Role::Jungle => "JUNGLE",
            Role::Middle => "MIDDLE",
            Role::Bottom => "BOTTOM",
            Role::Utility => "UTILITY",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// A patch ("16.23"). Ordered by (major, minor).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Patch {
    pub major: u16,
    pub minor: u16,
}

impl Patch {
    /// "16.23", "16.23.1" (Data Dragon) or "16.23.712.4471" (match-v5 gameVersion) → 16.23.
    pub fn parse(s: &str) -> Option<Patch> {
        let mut it = s.split('.');
        let major = it.next()?.trim().parse().ok()?;
        let minor = it.next()?.trim().parse().ok()?;
        Some(Patch { major, minor })
    }
}

impl fmt::Display for Patch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// A game's version: its patch and the hotfix build (the third part of match-v5's `gameVersion`, which changes when
/// Riot ships a mid-patch hotfix).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GameVersion {
    pub patch: Patch,
    pub build: u32,
}

impl GameVersion {
    pub fn parse(s: &str) -> Option<GameVersion> {
        let patch = Patch::parse(s)?;
        let build = s.split('.').nth(2).and_then(|b| b.trim().parse().ok()).unwrap_or(0);
        Some(GameVersion { patch, build })
    }
}

/// Platforms Build Fetcher reads (decision 4). Others can be added; `routing` says which budget they spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Platform {
    Eun1,
    Euw1,
    Kr,
}

impl Platform {
    pub const ALL: [Platform; 3] = [Platform::Eun1, Platform::Euw1, Platform::Kr];

    pub fn parse(s: &str) -> Option<Platform> {
        match s.to_ascii_uppercase().as_str() {
            "EUN1" => Some(Platform::Eun1),
            "EUW1" => Some(Platform::Euw1),
            "KR" => Some(Platform::Kr),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Eun1 => "EUN1",
            Platform::Euw1 => "EUW1",
            Platform::Kr => "KR",
        }
    }
    /// match-v5's regional routing value: EUN1 and EUW1 share EUROPE, KR uses ASIA.
    pub fn routing(self) -> Routing {
        match self {
            Platform::Eun1 | Platform::Euw1 => Routing::Europe,
            Platform::Kr => Routing::Asia,
        }
    }
}

/// A regional routing value (match-v5 host). Each has its own application rate limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Routing {
    Europe,
    Asia,
}

impl Routing {
    pub fn host(self) -> &'static str {
        match self {
            Routing::Europe => "europe.api.riotgames.com",
            Routing::Asia => "asia.api.riotgames.com",
        }
    }
}

/// The rank a game was sampled from. Master+ first; Diamond is widened to only for thin champion-role pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RankBand {
    MasterPlus,
    Diamond,
    /// The user's own games, whatever their rank.
    Mine,
}

/// The enemy team's damage mix, from its champions (Data Dragon tags and info) or, in game, their items.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DamageProfile {
    /// Mostly physical (>= 65% of the team's expected damage).
    Physical,
    /// Mostly magic (>= 65%).
    Magic,
    Mixed,
}

impl DamageProfile {
    /// One champion's expected share of magic damage, from Data Dragon's 0-10 `attack` and `magic` ratings. Squared,
    /// because the ratings are coarse and a 3/8 mage still deals almost all magic damage; None for an unknown champion.
    pub fn magic_share_of(data: &crate::ddragon::StaticData, champion: i64) -> Option<f64> {
        let c = data.champion(champion)?;
        let (a, m) = (c.info.attack * c.info.attack, c.info.magic * c.info.magic);
        Some(if a + m > 0.0 { m / (a + m) } else { 0.5 })
    }

    /// A team's mean magic share (unknown champions skipped); None when none is known.
    pub fn magic_share(data: &crate::ddragon::StaticData, champions: &[i64]) -> Option<f64> {
        let known: Vec<f64> = champions.iter().filter_map(|c| DamageProfile::magic_share_of(data, *c)).collect();
        (!known.is_empty()).then(|| known.iter().sum::<f64>() / known.len() as f64)
    }

    /// The profile of a magic share: >= 0.65 Magic, <= 0.35 Physical, else Mixed.
    pub fn from_magic_share(share: f64) -> DamageProfile {
        if share >= 0.65 {
            DamageProfile::Magic
        } else if share <= 0.35 {
            DamageProfile::Physical
        } else {
            DamageProfile::Mixed
        }
    }

    /// The enemy team's profile from its champions. The store's counts and the learner's contexts both use this, so
    /// a count's `enemy_profile` means the same thing on both sides.
    pub fn of_team(data: &crate::ddragon::StaticData, champions: &[i64]) -> Option<DamageProfile> {
        DamageProfile::magic_share(data, champions).map(DamageProfile::from_magic_share)
    }
}

// ------------------------------------------------------------------------------------------------ build options

/// One decision a build is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OptionKind {
    /// The whole rune page: primary tree, keystone and three runes, secondary tree and two runes, three shards.
    RunePage,
    /// The keystone alone (a coarser grouping of RunePage, for thin data).
    Keystone,
    /// Both summoner spells.
    Summoners,
    /// Which ability is maxed first, second, third ("Q>E>W").
    SkillMax,
    /// The first levels' skill-ups ("Q,E,W,Q,Q,R").
    SkillOrder,
    /// Items bought before 1:00.
    StartingItems,
    /// The first completed item.
    FirstItem,
    /// The first three completed items, in order.
    CoreItems,
    /// Tier-2 boots.
    Boots,
    /// A completed item bought 4th or later (late-game choices and swaps).
    LateItem,
}

impl OptionKind {
    pub const ALL: [OptionKind; 10] = [
        OptionKind::RunePage,
        OptionKind::Keystone,
        OptionKind::Summoners,
        OptionKind::SkillMax,
        OptionKind::SkillOrder,
        OptionKind::StartingItems,
        OptionKind::FirstItem,
        OptionKind::CoreItems,
        OptionKind::Boots,
        OptionKind::LateItem,
    ];
}

/// One value of one decision. `key` is canonical, so equal choices always compare equal:
///
/// | kind | key |
/// |---|---|
/// | RunePage | `8000:8010,9111,9104,8299/8400:8444,8473/5008,5008,5011` (style:perks/substyle:perks/shards) |
/// | Keystone | `8010` |
/// | Summoners | `4,12` (ascending) |
/// | SkillMax | `Q>E>W` |
/// | SkillOrder | `Q,E,W,Q,Q,R` (the first `SKILL_ORDER_LEVELS` levels) |
/// | StartingItems | `1055,2003` (ascending, repeated for several of one item) |
/// | FirstItem, LateItem, Boots | `3078` |
/// | CoreItems | `3078>3074>3053` |
///
/// Build keys with the constructors below, never by hand.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BuildOption {
    pub kind: OptionKind,
    pub key: String,
}

pub const SKILL_ORDER_LEVELS: usize = 6;

impl BuildOption {
    pub fn rune_page(style: i64, perks: &[i64], sub_style: i64, sub_perks: &[i64], shards: &[i64]) -> BuildOption {
        let j = |v: &[i64]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        BuildOption { kind: OptionKind::RunePage, key: format!("{style}:{}/{sub_style}:{}/{}", j(perks), j(sub_perks), j(shards)) }
    }
    pub fn keystone(perk: i64) -> BuildOption {
        BuildOption { kind: OptionKind::Keystone, key: perk.to_string() }
    }
    pub fn summoners(a: i64, b: i64) -> BuildOption {
        let (x, y) = if a <= b { (a, b) } else { (b, a) };
        BuildOption { kind: OptionKind::Summoners, key: format!("{x},{y}") }
    }
    /// `order` is the skill slot (1 = Q … 4 = R) of each level-up, in order.
    pub fn skill_max(order: &[u8]) -> Option<BuildOption> {
        let mut levels = [0u8; 3];
        let mut maxed: Vec<u8> = Vec::new();
        for s in order {
            if (1..=3).contains(s) {
                levels[(*s - 1) as usize] += 1;
                if levels[(*s - 1) as usize] == 5 {
                    maxed.push(*s);
                }
            }
        }
        // The ability with the most points by level 11 goes first when nothing is maxed yet (short games).
        if maxed.len() < 3 {
            let mut rest: Vec<u8> = (1..=3).filter(|s| !maxed.contains(s)).collect();
            rest.sort_by_key(|s| std::cmp::Reverse((levels[(*s - 1) as usize], 3 - *s)));
            maxed.extend(rest);
        }
        let n = order.iter().filter(|s| (1..=3).contains(*s)).count();
        (n >= 7).then(|| BuildOption { kind: OptionKind::SkillMax, key: maxed.iter().map(|s| slot_letter(*s)).collect::<Vec<_>>().join(">") })
    }
    pub fn skill_order(order: &[u8]) -> Option<BuildOption> {
        (order.len() >= SKILL_ORDER_LEVELS).then(|| BuildOption {
            kind: OptionKind::SkillOrder,
            key: order[..SKILL_ORDER_LEVELS].iter().map(|s| slot_letter(*s)).collect::<Vec<_>>().join(","),
        })
    }
    pub fn starting_items(items: &[i64]) -> Option<BuildOption> {
        let mut v = items.to_vec();
        v.sort_unstable();
        (!v.is_empty()).then(|| BuildOption { kind: OptionKind::StartingItems, key: v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",") })
    }
    pub fn first_item(item: i64) -> BuildOption {
        BuildOption { kind: OptionKind::FirstItem, key: item.to_string() }
    }
    pub fn core_items(items: &[i64]) -> Option<BuildOption> {
        (items.len() >= 3).then(|| BuildOption { kind: OptionKind::CoreItems, key: items[..3].iter().map(|x| x.to_string()).collect::<Vec<_>>().join(">") })
    }
    pub fn boots(item: i64) -> BuildOption {
        BuildOption { kind: OptionKind::Boots, key: item.to_string() }
    }
    pub fn late_item(item: i64) -> BuildOption {
        BuildOption { kind: OptionKind::LateItem, key: item.to_string() }
    }

    /// The item ids in the key (items kinds), in order.
    pub fn item_ids(&self) -> Vec<i64> {
        match self.kind {
            OptionKind::StartingItems | OptionKind::FirstItem | OptionKind::CoreItems | OptionKind::Boots | OptionKind::LateItem => {
                self.key.split([',', '>']).filter_map(|x| x.parse().ok()).collect()
            }
            _ => vec![],
        }
    }
}

fn slot_letter(s: u8) -> &'static str {
    match s {
        1 => "Q",
        2 => "W",
        3 => "E",
        4 => "R",
        _ => "?",
    }
}

// ------------------------------------------------------------------------------------------------ match records

/// One player of a stored match, as the store keeps it (compact; raw API answers are discarded).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlayerRecord {
    pub champion: i64,
    /// None only in records that are never stored (role assignment failed → the match is dropped).
    pub role: Option<Role>,
    /// 100 or 200.
    pub team: u16,
    pub win: bool,
    /// The user's own account.
    pub is_me: bool,
    pub summoners: [i64; 2],
    /// primary style, 4 primary perks, sub style, 2 sub perks, 3 shards.
    pub rune_style: i64,
    pub perks: [i64; 4],
    pub rune_sub_style: i64,
    pub sub_perks: [i64; 2],
    pub shards: [i64; 3],
    /// Final inventory (slots 0-6), 0 for empty.
    pub final_items: [i64; 7],
    pub kills: u16,
    pub deaths: u16,
    pub assists: u16,
    pub gold_earned: u32,
    pub cs: u16,
    pub champ_level: u8,
    pub physical_damage_to_champions: u32,
    pub magic_damage_to_champions: u32,
    pub true_damage_to_champions: u32,
    /// From the timeline (empty without one): purchases as (item, seconds), undos already applied, sells kept as
    /// negative ids.
    pub purchases: Vec<(i64, u32)>,
    /// From the timeline: skill slot (1-4) of each level-up, in order.
    pub skill_order: Vec<u8>,
}

/// The game state of one player at one minute (from the timeline's participantFrames).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    pub minute: u16,
    pub total_gold: u32,
    pub cs: u16,
    pub level: u8,
    pub xp: u32,
}

/// Team events with a time, from the timeline (kills, towers, dragons, barons, heralds, inhibitors).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamEventKind {
    Kill,
    Tower,
    Inhibitor,
    Dragon,
    Herald,
    Baron,
    Grubs,
    Atakhan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamEvent {
    pub second: u32,
    /// The team credited (100 or 200).
    pub team: u16,
    pub kind: TeamEventKind,
}

/// A whole match as the store keeps it: one record per match id, whichever player and region it was found from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MatchRecord {
    /// "EUW1_7234567890".
    pub match_id: String,
    pub platform: Option<Platform>,
    pub version: Option<GameVersion>,
    pub rank: Option<RankBand>,
    /// gameStartTimestamp (ms).
    pub start_ms: i64,
    pub duration_s: u32,
    /// Ten players, team 100 first, each team in Role::ALL order.
    pub players: Vec<PlayerRecord>,
    /// Empty without a timeline. frames[i] are players[i]'s, one per minute.
    pub frames: Vec<Vec<Frame>>,
    pub events: Vec<TeamEvent>,
}

impl MatchRecord {
    pub fn has_timeline(&self) -> bool {
        !self.frames.is_empty()
    }
    /// The player in `role` on the other team.
    pub fn lane_opponent(&self, i: usize) -> Option<&PlayerRecord> {
        let me = self.players.get(i)?;
        self.players.iter().find(|p| p.team != me.team && p.role.is_some() && p.role == me.role)
    }
}

// ------------------------------------------------------------------------------------------------ tier B counts

/// What a count is conditioned on. `None` is "any" (the champion-role total).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CountContext {
    pub lane_opponent: Option<i64>,
    pub enemy_profile: Option<DamageProfile>,
}

/// Counts of one option for one champion-role, patch, rank band and context. `expected_wins` is the sum of the
/// baseline win chance of each game (the model's, or the champion-role's own rate when no model exists yet), so
/// `wins - expected_wins` is the win probability added (WPA) summed over games.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionCounts {
    pub patch: Patch,
    pub rank: RankBand,
    pub context: CountContext,
    pub option: BuildOption,
    pub games: u32,
    pub wins: u32,
    pub expected_wins: f64,
}

/// A request for counts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountQuery {
    pub champion: i64,
    pub role: Role,
    /// Patches to include (newest first). Empty = every stored patch.
    pub patches: Vec<Patch>,
    /// Kinds to include. Empty = all.
    pub kinds: Vec<OptionKind>,
}

/// Games stored for one champion-role (for "thin data" decisions and the dashboard).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairVolume {
    pub champion: i64,
    pub role: Role,
    pub patch: Patch,
    pub games: u32,
    pub wins: u32,
    /// Newest game's start (ms).
    pub newest_ms: i64,
}

/// What the learner reads from the store. The store implements it over its database; tests implement it in memory.
pub trait DataSource: Send + Sync {
    /// Tier B counts matching the query (every rank band and context; the caller weighs them).
    fn counts(&self, q: &CountQuery) -> anyhow::Result<Vec<OptionCounts>>;
    /// Games per champion-role and patch.
    fn volumes(&self, patches: &[Patch]) -> anyhow::Result<Vec<PairVolume>>;
    /// Stored match records with a sequence number greater than `after_seq`, oldest first, at most `limit`.
    /// Records carry timelines for tier A champions and the tier B sample. Returns (seq, record).
    fn records_after(&self, after_seq: u64, limit: usize) -> anyhow::Result<Vec<(u64, MatchRecord)>>;
    /// The fixed-size held-out sample guardrails evaluate on (never returned by `records_after`).
    fn validation_records(&self) -> anyhow::Result<Vec<MatchRecord>>;
    /// The newest patch with data.
    fn current_patch(&self) -> anyhow::Result<Option<Patch>>;
    /// Stored records with a timeline (1.3.0: the team network's wake-up threshold counts them).
    fn timeline_count(&self) -> anyhow::Result<u64> {
        Ok(0)
    }
}

// ------------------------------------------------------------------------------------------------ recommendations

/// The draft as champ select shows it. Hidden names are never part of it: only champions and roles.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DraftContext {
    pub champion: i64,
    pub role: Option<Role>,
    /// Allies' locked or hovered champions (without the user), with their assigned roles when known.
    pub allies: Vec<(i64, Option<Role>)>,
    pub enemies: Vec<(i64, Option<Role>)>,
    pub patch: Option<Patch>,
}

/// The game as the Live Client Data API shows it, from the user's side.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LiveContext {
    pub draft: DraftContext,
    pub game_time_s: f64,
    /// The user's items (ids), and every enemy's (by champion).
    pub my_items: Vec<i64>,
    pub enemy_items: Vec<(i64, Vec<i64>)>,
    pub my_level: u8,
    pub my_cs: u16,
    pub my_gold: f64,
    /// The lane opponent's level and CS when the Live Client shows them (it shows every player's).
    pub opponent_level: Option<u8>,
    pub opponent_cs: Option<u16>,
    pub team_kills: u16,
    pub enemy_kills: u16,
    pub towers: (u8, u8),
    pub dragons: (u8, u8),
    pub barons: (u8, u8),
    /// Every player but the user (1.3.0): champion, side, level and items, as the scoreboard shows them. The
    /// combat math and the team network read it.
    #[serde(default)]
    pub players: Vec<LivePlayer>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LivePlayer {
    pub champion: i64,
    pub role: Option<Role>,
    /// On the user's team.
    pub ally: bool,
    pub level: u8,
    pub items: Vec<i64>,
}

/// Where a number came from when the matchup's own data were thin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Basis {
    /// The lane opponent's games.
    Matchup,
    /// Games against the enemy team's damage profile.
    Profile,
    /// All games of this champion in this role.
    Lane,
    /// All games of this champion.
    Champion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tier {
    A,
    B,
}

/// One ranked choice, with the evidence behind it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankedOption {
    pub option: BuildOption,
    /// Display text ("Conqueror", "Flash + Ignite", "Q > E > W", item names).
    pub label: String,
    /// Plain counts (recency- and rank-weighted games are in `weighted_games`).
    pub games: u32,
    pub wins: u32,
    pub weighted_games: f64,
    /// Shrunk win rate (0-1) and win probability added (percentage points / 100) against the baseline.
    pub win_rate: f64,
    pub wpa: f64,
    /// The model's predicted win chance with this option in a context like the user's, when a model exists.
    pub predicted_win: Option<f64>,
    pub basis: Basis,
    /// Fewer games than the section's minimum: shown, but flagged.
    pub thin: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub kind: OptionKind,
    /// Best first.
    pub options: Vec<RankedOption>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildRecommendation {
    pub champion: i64,
    pub role: Role,
    pub patch: Option<Patch>,
    pub tier: Tier,
    pub lane_opponent: Option<i64>,
    pub enemy_profile: Option<DamageProfile>,
    /// Games behind the champion-role total (all patches weighed in).
    pub games: u32,
    pub sections: Vec<Section>,
    /// The model version used, if any.
    pub model: Option<String>,
    /// One line per caveat ("Thin data: 41 games; matchup falls back to lane").
    pub notes: Vec<String>,
}

/// In game: the next purchases worth considering, best first. Several options are always given (never a single
/// instruction): Riot's policy asks products to highlight decisions and leave them to the player.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveAdvice {
    pub next_buys: Vec<RankedOption>,
    /// Items that answer the enemy's build (armor, magic resist, anti-heal…), with a reason each.
    pub counters: Vec<(RankedOption, String)>,
    pub predicted_win: Option<f64>,
    pub notes: Vec<String>,
    /// 1.2.0: for each item in `next_buys` and `counters`, what it costs from here with the user's gold, and why it
    /// fits this game.
    #[serde(default)]
    pub buys: Vec<BuyNote>,
    /// 1.5.0: the build in buying order from where the user stands: the starting items while the game is young, the
    /// items already built, then each next buy until the build is full. Recomputed with every update, so it follows
    /// the user's gold, the components they already bought, and the enemy's (and the lane opponent's) items.
    #[serde(default)]
    pub plan: Option<BuildPlan>,
}

/// The build in buying order (1.5.0).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuildPlan {
    /// The starting items, while the game is in its first two minutes and none of them is bought yet.
    pub start: Option<RankedOption>,
    /// Completed items and boots already owned, in inventory order.
    pub done: Vec<i64>,
    /// What is left to buy, in order: the first step is the next buy. At most enough to fill six slots.
    pub steps: Vec<PlanStep>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub item: i64,
    pub label: String,
    /// Its price from here: components already owned count as paid; the gold missing; the part to buy now.
    pub buy: BuyNote,
    /// Why it is here, and here in the order ("Core item 2 (812 games)", "Enemy damage is mostly magic").
    pub why: String,
    /// It answers something in the enemy's (or the lane opponent's) items.
    pub counter: bool,
    /// Other items that fill this slot well, best first (the choice stays the player's).
    pub alternatives: Vec<i64>,
}

/// An item's price from where the user stands. Only what the player can see goes in: their own gold and items, and
/// the enemy's items as the scoreboard shows them (never the enemy's gold, which the game doesn't show).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuyNote {
    pub item: i64,
    /// Gold still to pay for the whole item, counting the components already owned.
    pub cost: u32,
    /// Gold missing for it (0: it can be bought now).
    pub short: u32,
    /// When it can't be bought whole: the most expensive part of its recipe the gold covers now, and that part's price.
    pub part: Option<i64>,
    pub part_cost: Option<u32>,
    /// Why it fits this game, from the enemy's items ("4 enemy armor items"), when something stands out.
    pub reason: Option<String>,
}

/// What the server asks for a build. The learner implements it.
pub trait BuildSource: Send + Sync {
    fn recommend(&self, ctx: &DraftContext) -> anyhow::Result<BuildRecommendation>;
    fn live(&self, ctx: &LiveContext) -> anyhow::Result<LiveAdvice>;
}

// ------------------------------------------------------------------------------------------------ status

/// What the user is doing: heavy work and fetching pause outside Idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Activity {
    #[default]
    Idle,
    /// The PC is in use (not idle) but no League game or champ select.
    Busy,
    ChampSelect,
    InGame,
}

/// One routing value's budget as the fetcher sees it (the dashboard meter).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStatus {
    /// "EUROPE", "ASIA", or a platform host ("euw1").
    pub scope: String,
    /// Riot's limits for the key ("20:1,100:120"), as last seen.
    pub limits: String,
    /// Requests Riot counted in the longest window (X-App-Rate-Limit-Count), and how many of them were ours.
    pub used: u32,
    pub ours: u32,
    pub limit: u32,
    pub window_s: u32,
    /// Another app (e.g. Irelia Companion) is spending the same key's budget.
    pub shared: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchStatus {
    pub running: bool,
    /// Why fetching is paused, if it is ("champ select", "in game", "no API key", "key rejected").
    pub paused: Option<String>,
    pub budgets: Vec<BudgetStatus>,
    pub requests_last_hour: u32,
    pub matches_last_hour: u32,
    pub matches_total: u64,
    /// Dropped answers and games by reason ("remake", "leaver", "roles", "malformed", "duplicate", "queue").
    pub dropped: Vec<(String, u64)>,
    /// The work item being done now ("my games", "tier A: Ahri MIDDLE", "thin: Ivern JUNGLE").
    pub current: Option<String>,
    pub key_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatus {
    pub budget_bytes: u64,
    pub used_bytes: u64,
    /// (part, bytes): "counts", "records", "validation", "checkpoints", "cache", "ddragon", "log".
    pub parts: Vec<(String, u64)>,
    pub patches: Vec<Patch>,
    pub last_cleanup_freed_bytes: Option<u64>,
}

/// Held-out metrics of one model version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    /// Examples scored: one player at one minute of a test match (up to 70 per match). Despite the name, not matches:
    /// see `matches`.
    pub games: u32,
    /// Distinct test matches behind the numbers (0 in checkpoints written before 1.1.1).
    #[serde(default)]
    pub matches: u32,
    pub log_loss: f64,
    pub brier: f64,
    /// Expected calibration error per 10-minute window of game time (0-10, 10-20, 20-30, 30+).
    pub calibration: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointInfo {
    pub version: String,
    /// "logistic" or "shared-nn".
    pub kind: String,
    pub created_ms: i64,
    pub metrics: Metrics,
    pub records_seen: u64,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub learning: bool,
    /// "cpu" or "gpu: NVIDIA GeForce …".
    pub backend: String,
    pub checkpoints: Vec<CheckpointInfo>,
    /// The newest lines of the learning log ("v12 rejected: log loss 0.6712 > 0.6705").
    pub log: Vec<String>,
    /// The team network (step 3, 1.3.1): dormant with its progress, or awake.
    #[serde(default)]
    pub team: Option<TeamProgress>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TeamProgress {
    pub awake: bool,
    /// Games with a timeline so far (stored or learnt), and the number that wakes it up.
    pub games: u64,
    pub threshold: u64,
    /// The version in force, once one passed a check.
    pub in_force: Option<String>,
    /// Stored games it still has to go through, once awake (approximate).
    pub backlog: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_keys_are_canonical() {
        assert_eq!(BuildOption::summoners(12, 4), BuildOption::summoners(4, 12));
        assert_eq!(BuildOption::summoners(12, 4).key, "4,12");
        assert_eq!(BuildOption::starting_items(&[2003, 1055]).unwrap().key, "1055,2003");
        assert_eq!(BuildOption::core_items(&[3078, 3074, 3053, 3026]).unwrap().key, "3078>3074>3053");
        assert_eq!(BuildOption::core_items(&[3078, 3074, 3053]).unwrap().item_ids(), vec![3078, 3074, 3053]);
        assert!(BuildOption::core_items(&[3078]).is_none());
        assert_eq!(
            BuildOption::rune_page(8000, &[8010, 9111, 9104, 8299], 8400, &[8444, 8473], &[5008, 5008, 5011]).key,
            "8000:8010,9111,9104,8299/8400:8444,8473/5008,5008,5011"
        );
    }

    #[test]
    fn skill_max_and_order() {
        // Q E W Q Q R Q E Q E R E E W W R W W
        let o = [1, 3, 2, 1, 1, 4, 1, 3, 1, 3, 4, 3, 3, 2, 2, 4, 2, 2];
        assert_eq!(BuildOption::skill_max(&o).unwrap().key, "Q>E>W");
        assert_eq!(BuildOption::skill_order(&o).unwrap().key, "Q,E,W,Q,Q,R");
        assert!(BuildOption::skill_max(&o[..5]).is_none());
        // A short game: Q 4 points, E 3, W 1 by level 9.
        assert_eq!(BuildOption::skill_max(&[1, 3, 2, 1, 1, 4, 1, 3, 3]).unwrap().key, "Q>E>W");
    }

    #[test]
    fn versions_and_routing() {
        let v = GameVersion::parse("16.23.712.4471").unwrap();
        assert_eq!(v.patch, Patch { major: 16, minor: 23 });
        assert_eq!(v.build, 712);
        assert_eq!(v.patch.to_string(), "16.23");
        assert!(Patch::parse("16.9") < Patch::parse("16.10"));
        assert_eq!(Platform::Eun1.routing(), Platform::Euw1.routing());
        assert_eq!(Platform::Kr.routing(), Routing::Asia);
        assert_eq!(Role::from_riot("utility"), Some(Role::Utility));
        assert_eq!(Role::from_riot(""), None);
        assert_eq!(serde_json::to_string(&Role::Utility).unwrap(), "\"UTILITY\"");
    }
}
