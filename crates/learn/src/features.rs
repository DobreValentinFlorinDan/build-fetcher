//! What the models see: a `Situation` (one player of one match at one minute) and its label. Situations are kept
//! semantic (champions, rune page, items in order, game state) rather than as feature vectors, so the recommender can
//! swap one decision (another keystone, another first item) and ask the model again.

use bf_core::build::{BuildOption, DamageProfile, GameVersion, MatchRecord, Patch, RankBand, Role, TeamEventKind};
use bf_core::ddragon::StaticData;
use bf_core::engine::items::{is_completed_item, is_tier2_boots};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// The minutes a record with a timeline is sampled at: two in each 10-minute calibration window up to 40, so every
/// window has examples (0-10: 2, 8; 10-20: 14; 20-30: 20, 26; 30+: 32, 38).
pub const MINUTES: [u16; 7] = [2, 8, 14, 20, 26, 32, 38];

/// The game state at a minute, from the player's side (positive = ahead).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct GameState {
    pub minute: f64,
    /// Against the lane opponent.
    pub gold_diff: f64,
    pub cs_diff: f64,
    pub level_diff: f64,
    /// Team totals, own minus enemy.
    pub kill_diff: f64,
    pub tower_diff: f64,
    pub dragon_diff: f64,
    pub baron_diff: f64,
}

impl GameState {
    /// The dense inputs both models use, scaled to about unit size. Time interactions let an early gold lead count for
    /// less than a late one.
    pub fn dense(&self) -> [f64; DENSE] {
        let t = (self.minute / 40.0).min(1.5);
        let g = self.gold_diff / 1000.0;
        [
            self.minute / 10.0,
            g,
            self.cs_diff / 20.0,
            self.level_diff / 2.0,
            self.kill_diff / 5.0,
            self.tower_diff / 2.0,
            self.dragon_diff / 2.0,
            self.baron_diff,
            g * t,
            self.kill_diff / 5.0 * t,
            self.tower_diff / 2.0 * t,
        ]
    }
}

pub const DENSE: usize = 11;

/// One player at one minute: everything a prediction depends on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Situation {
    pub champion: i64,
    pub role: Option<Role>,
    pub opponent: Option<i64>,
    pub allies: Vec<i64>,
    pub enemies: Vec<i64>,
    pub profile: Option<DamageProfile>,
    /// 0 when unknown.
    pub keystone: i64,
    pub sub_style: i64,
    /// `BuildOption` keys (canonical), None when unknown.
    pub rune_page: Option<String>,
    pub summoners: Option<String>,
    pub skill_max: Option<String>,
    pub starting: Option<String>,
    /// Completed items owned at this minute, in purchase order (boots apart).
    pub items: Vec<i64>,
    pub boots: Option<i64>,
    pub state: GameState,
    /// The enemy team's completed items and boots at this minute (what the Tab scoreboard shows), and what they add up
    /// to (`enemy_tag_levels`). Empty before 1.2.0 and without a timeline.
    #[serde(default)]
    pub enemy_items: Vec<i64>,
    #[serde(default)]
    pub enemy_tags: [f64; ENEMY_TAGS],
    /// The combat vector (1.3.0, `combat::combat_vector`): the build's stats and what they do against the enemy's.
    /// Empty without a timeline, and in the stored draft contexts.
    #[serde(default)]
    pub combat: Vec<f32>,
}

/// What an enemy team's items add up to, one level per tag: armor, magic resist, healing (life steal, omnivamp),
/// anti-heal, ability power, attack damage (crit included) and health.
pub const ENEMY_TAGS: usize = 7;
pub const TAG_NAMES: [&str; ENEMY_TAGS] = ["armor", "magic resist", "healing", "anti-heal", "ability power", "attack damage", "health"];
/// A tag's level is its share of the team's build items, counted against at least this many items: a share does
/// not grow with game time (a count would, and then acts as a clock: on synthetic data it left late-game predictions
/// overconfident), and the floor keeps one early item from reading as a whole build.
const TAG_MIN_ITEMS: f64 = 8.0;
/// Shares times this, so a typical level is about 1 (a quarter of a build): the weights it needs stay small next to
/// the L2 pull, which a rarely seen item × tag pair would otherwise not overcome.
const TAG_SCALE: f64 = 4.0;

/// A training or validation example.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub situation: Situation,
    pub won: bool,
    pub is_me: bool,
    pub version: Option<GameVersion>,
    pub rank: Option<RankBand>,
    /// The match and team: examples of one group share their label (the calibration noise floor counts them as one).
    pub group: u64,
    /// The match alone: its two teams' labels are each other's opposite, so for the calibration's standard error a
    /// match is one independent piece of evidence.
    #[serde(default)]
    pub game: u64,
}

impl Sample {
    pub fn label(&self) -> f64 {
        if self.won {
            1.0
        } else {
            0.0
        }
    }
    /// The calibration window (0: 0-10 min, 1: 10-20, 2: 20-30, 3: 30+).
    pub fn window(&self) -> usize {
        window_of(self.situation.state.minute)
    }
}

pub fn window_of(minute: f64) -> usize {
    ((minute / 10.0).floor().max(0.0) as usize).min(3)
}

/// Which items count as completed and which as tier-2 boots (from Data Dragon, worked out once).
#[derive(Clone, Debug, Default)]
pub struct ItemClasses {
    pub completed: HashSet<i64>,
    pub boots: HashSet<i64>,
    /// Enemy-build tags per completed item and boots (bit k: `TAG_NAMES[k]`).
    pub tags: std::collections::HashMap<i64, u8>,
}

impl ItemClasses {
    pub fn new(data: &StaticData) -> ItemClasses {
        let mut c = ItemClasses::default();
        for it in data.items.values() {
            if is_completed_item(Some(it)) {
                c.completed.insert(it.id);
            } else if is_tier2_boots(Some(it)) {
                c.boots.insert(it.id);
            } else {
                continue;
            }
            let has = |t: &str| it.tags.iter().any(|x| x == t);
            let bits = [
                it.armor > 0.0,
                it.magic_resist > 0.0,
                has("LifeSteal") || has("SpellVamp"),
                it.anti_heal,
                has("SpellDamage"),
                has("Damage") || has("CriticalStrike"),
                has("Health"),
            ];
            let mask = bits.iter().enumerate().fold(0u8, |m, (k, b)| if *b { m | (1 << k) } else { m });
            c.tags.insert(it.id, mask);
        }
        c
    }

    /// Completed items and boots only (what counts as an enemy's build).
    pub fn is_build_item(&self, id: i64) -> bool {
        self.completed.contains(&id) || self.boots.contains(&id)
    }

    /// The levels of `ENEMY_TAGS` that `items` (one team's) add up to.
    pub fn enemy_tag_levels(&self, items: &[i64]) -> [f64; ENEMY_TAGS] {
        let mut n = [0.0; ENEMY_TAGS];
        let mut total = 0.0;
        for id in items {
            let Some(m) = self.tags.get(id).copied() else { continue };
            total += 1.0;
            for (k, x) in n.iter_mut().enumerate() {
                if m & (1 << k) != 0 {
                    *x += 1.0;
                }
            }
        }
        let of = f64::max(total, TAG_MIN_ITEMS);
        n.map(|x| TAG_SCALE * x / of)
    }
}

/// Completed items and boots owned at second `at`, from a purchase log (sells are negative ids).
pub fn owned_at(purchases: &[(i64, u32)], at: u32, classes: &ItemClasses) -> (Vec<i64>, Option<i64>) {
    let mut items: Vec<i64> = Vec::new();
    let mut boots: Option<i64> = None;
    for &(id, s) in purchases {
        if s > at {
            break;
        }
        if id < 0 {
            let id = -id;
            if let Some(p) = items.iter().position(|x| *x == id) {
                items.remove(p);
            }
            if boots == Some(id) {
                boots = None;
            }
        } else if classes.boots.contains(&id) {
            boots = Some(id);
        } else if classes.completed.contains(&id) {
            items.push(id);
        }
    }
    (items, boots)
}

/// The decisions a record shows for one player, as `BuildOption`s (what the store counts too).
pub fn options_of(r: &MatchRecord, i: usize, classes: &ItemClasses) -> Vec<BuildOption> {
    let p = &r.players[i];
    let mut out = Vec::new();
    if p.rune_style != 0 {
        out.push(BuildOption::rune_page(p.rune_style, &p.perks, p.rune_sub_style, &p.sub_perks, &p.shards));
        out.push(BuildOption::keystone(p.perks[0]));
    }
    if p.summoners[0] != 0 {
        out.push(BuildOption::summoners(p.summoners[0], p.summoners[1]));
    }
    out.extend(BuildOption::skill_max(&p.skill_order));
    out.extend(BuildOption::skill_order(&p.skill_order));
    let start: Vec<i64> = p.purchases.iter().filter(|(id, s)| *s < 60 && *id > 0).map(|(id, _)| *id).collect();
    out.extend(BuildOption::starting_items(&start));
    let (items, boots) = owned_at(&p.purchases, u32::MAX, classes);
    // The order things were first bought in, sold or not: a swap later doesn't change what was built first.
    let mut first_bought: Vec<i64> = Vec::new();
    for &(id, _) in &p.purchases {
        if id > 0 && classes.completed.contains(&id) && !first_bought.contains(&id) {
            first_bought.push(id);
        }
    }
    let order = if first_bought.is_empty() { items } else { first_bought };
    if let Some(f) = order.first() {
        out.push(BuildOption::first_item(*f));
    }
    out.extend(BuildOption::core_items(&order));
    for it in order.iter().skip(3) {
        out.push(BuildOption::late_item(*it));
    }
    if let Some(b) = boots.or_else(|| p.purchases.iter().find(|(id, _)| classes.boots.contains(id)).map(|x| x.0)) {
        out.push(BuildOption::boots(b));
    }
    out
}

/// The draft part of a situation (no items, minute 0) for player `i`.
fn draft_situation(r: &MatchRecord, i: usize, data: &StaticData) -> Situation {
    let p = &r.players[i];
    let allies: Vec<i64> = r.players.iter().enumerate().filter(|(j, q)| *j != i && q.team == p.team).map(|(_, q)| q.champion).collect();
    let enemies: Vec<i64> = r.players.iter().filter(|q| q.team != p.team).map(|q| q.champion).collect();
    let skill_max = BuildOption::skill_max(&p.skill_order).map(|o| o.key);
    let start: Vec<i64> = p.purchases.iter().filter(|(id, s)| *s < 60 && *id > 0).map(|(id, _)| *id).collect();
    Situation {
        champion: p.champion,
        role: p.role,
        opponent: r.lane_opponent(i).map(|o| o.champion),
        profile: DamageProfile::of_team(data, &enemies),
        allies,
        enemies,
        keystone: p.perks[0],
        sub_style: p.rune_sub_style,
        rune_page: (p.rune_style != 0).then(|| BuildOption::rune_page(p.rune_style, &p.perks, p.rune_sub_style, &p.sub_perks, &p.shards).key),
        summoners: (p.summoners[0] != 0).then(|| BuildOption::summoners(p.summoners[0], p.summoners[1]).key),
        skill_max,
        starting: BuildOption::starting_items(&start).map(|o| o.key),
        items: vec![],
        boots: None,
        state: GameState::default(),
        enemy_items: vec![],
        enemy_tags: [0.0; ENEMY_TAGS],
        combat: vec![],
    }
}

fn team_events_until(r: &MatchRecord, team: u16, second: u32) -> [f64; 4] {
    // kills, towers, dragons, barons: own minus enemy
    let mut d = [0.0; 4];
    for e in &r.events {
        if e.second > second {
            continue;
        }
        let k = match e.kind {
            TeamEventKind::Kill => 0,
            TeamEventKind::Tower => 1,
            TeamEventKind::Dragon => 2,
            TeamEventKind::Baron => 3,
            _ => continue,
        };
        d[k] += if e.team == team { 1.0 } else { -1.0 };
    }
    d
}

/// Every example a record gives: each player with a role at each sampled minute when there is a timeline, else one
/// draft-only example per player at minute 0 (what champ select knows).
pub fn samples_of(r: &MatchRecord, data: &StaticData, classes: &ItemClasses) -> Vec<Sample> {
    let mut out = Vec::new();
    // What every player owns at each sampled minute (each player's enemies need it too).
    let owned: Vec<Vec<(Vec<i64>, Option<i64>)>> =
        r.players.iter().map(|q| MINUTES.iter().map(|m| if r.has_timeline() { owned_at(&q.purchases, *m as u32 * 60, classes) } else { (vec![], None) }).collect()).collect();
    for (i, p) in r.players.iter().enumerate() {
        if p.role.is_none() || p.champion <= 0 {
            continue;
        }
        let base = draft_situation(r, i, data);
        let group = hash(&[key_id(&r.match_id), p.team as i64]);
        let game = hash(&[key_id(&r.match_id)]);
        let make = |situation: Situation| Sample { situation, won: p.win, is_me: p.is_me, version: r.version, rank: r.rank, group, game };
        if !r.has_timeline() || r.frames.get(i).map(|f| f.is_empty()).unwrap_or(true) {
            out.push(make(base));
            continue;
        }
        let opp = r.players.iter().position(|q| q.team != p.team && q.role == p.role);
        for (k, &m) in MINUTES.iter().enumerate() {
            let at = m as u32 * 60;
            if at + 60 > r.duration_s {
                break;
            }
            let Some(f) = r.frames[i].get(m as usize) else { break };
            let of = opp.and_then(|j| r.frames.get(j)).and_then(|fr| fr.get(m as usize));
            let (items, boots) = owned[i][k].clone();
            let ev = team_events_until(r, p.team, at);
            let mut s = base.clone();
            s.items = items;
            s.boots = boots;
            s.enemy_items = r
                .players
                .iter()
                .enumerate()
                .filter(|(_, q)| q.team != p.team)
                .flat_map(|(j, _)| owned[j][k].0.iter().copied().chain(owned[j][k].1))
                .collect();
            s.enemy_tags = classes.enemy_tag_levels(&s.enemy_items);
            // The combat math: everyone's level from the frames, items from the purchase logs.
            let level = |j: usize| r.frames.get(j).and_then(|fr| fr.get(m as usize)).map(|f| f.level as f64).unwrap_or(1.0);
            let kit: Vec<Vec<i64>> = (0..r.players.len()).map(|j| owned[j][k].0.iter().copied().chain(owned[j][k].1).collect()).collect();
            let enemies: Vec<crate::combat::PlayerIn> = r
                .players
                .iter()
                .enumerate()
                .filter(|(_, q)| q.team != p.team && q.champion > 0)
                .map(|(j, q)| crate::combat::PlayerIn { champion: q.champion, level: level(j), items: &kit[j] })
                .collect();
            s.combat = crate::combat::combat_vector(data, &crate::combat::PlayerIn { champion: p.champion, level: level(i), items: &kit[i] }, &enemies);
            s.state = GameState {
                minute: m as f64,
                gold_diff: of.map(|o| f.total_gold as f64 - o.total_gold as f64).unwrap_or(0.0),
                cs_diff: of.map(|o| f.cs as f64 - o.cs as f64).unwrap_or(0.0),
                level_diff: of.map(|o| f.level as f64 - o.level as f64).unwrap_or(0.0),
                kill_diff: ev[0],
                tower_diff: ev[1],
                dragon_diff: ev[2],
                baron_diff: ev[3],
            };
            out.push(make(s));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------ weights

/// How much a training example counts: the user's own games, newer patches and hotfixes, and higher ranks count more.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeightOptions {
    /// The user's own games (`is_me`).
    pub me: f64,
    /// Per patch of age: weight × this.
    pub patch_decay: f64,
    /// Same patch, an older hotfix build than the newest seen.
    pub old_hotfix: f64,
    pub master_plus: f64,
    pub diamond: f64,
}

impl Default for WeightOptions {
    fn default() -> Self {
        WeightOptions { me: 3.0, patch_decay: 0.75, old_hotfix: 0.85, master_plus: 1.0, diamond: 0.7 }
    }
}

/// The patches and hotfix builds the learner has seen, newest last: a game's age is counted in patches that exist,
/// not in version arithmetic (16.24 → 17.1 is one patch).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PatchClock {
    pub patches: Vec<Patch>,
    /// Newest hotfix build seen per patch ("16.23" → 712).
    pub builds: BTreeMap<String, u32>,
}

impl PatchClock {
    pub fn observe(&mut self, v: GameVersion) {
        if let Err(at) = self.patches.binary_search(&v.patch) {
            self.patches.insert(at, v.patch);
        }
        let b = self.builds.entry(v.patch.to_string()).or_insert(0);
        *b = (*b).max(v.build);
    }
    pub fn observe_patch(&mut self, p: Patch) {
        if let Err(at) = self.patches.binary_search(&p) {
            self.patches.insert(at, p);
        }
    }
    /// Patches newer than `p` that have been seen.
    pub fn age(&self, p: Patch) -> usize {
        self.patches.iter().filter(|x| **x > p).count()
    }
    pub fn weight(&self, v: Option<GameVersion>, o: &WeightOptions) -> f64 {
        let Some(v) = v else { return o.patch_decay };
        let mut w = bf_core::jsmath::pow(o.patch_decay, self.age(v.patch) as f64);
        if self.builds.get(&v.patch.to_string()).map(|b| v.build < *b).unwrap_or(false) {
            w *= o.old_hotfix;
        }
        w
    }
}

pub fn rank_weight(rank: Option<RankBand>, o: &WeightOptions) -> f64 {
    match rank {
        Some(RankBand::MasterPlus) | Some(RankBand::Mine) => o.master_plus,
        Some(RankBand::Diamond) => o.diamond,
        None => o.diamond,
    }
}

pub fn sample_weight(s: &Sample, clock: &PatchClock, o: &WeightOptions) -> f64 {
    let me = if s.is_me { o.me } else { 1.0 };
    me * clock.weight(s.version, o) * rank_weight(s.rank, o)
}

// ------------------------------------------------------------------------------------------------ hashing

/// FNV-1a then a SplitMix finaliser: stable across platforms and releases (checkpoints depend on it).
pub fn hash(parts: &[i64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    mix(h)
}

pub fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A key string as one number for hashing.
pub fn key_id(s: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h as i64
}

/// Deterministic random numbers (SplitMix64): shuffles, initial weights and the synthetic data.
#[derive(Clone, Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix(self.0)
    }
    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() >> 11) % n.max(1) as u64) as usize
    }
    /// Standard normal (Box-Muller with jsmath, so the same on every platform).
    pub fn normal(&mut self) -> f64 {
        let u = 1.0 - self.uniform();
        let v = self.uniform();
        (-2.0 * bf_core::jsmath::log(u)).sqrt() * cos_2pi(v)
    }
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

/// cos(2πv) from a Taylor series on [-π, π] (std's cos isn't guaranteed to round the same everywhere).
fn cos_2pi(v: f64) -> f64 {
    let x = 2.0 * std::f64::consts::PI * (v - 0.5);
    let x2 = x * x;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..20 {
        term *= -x2 / ((2 * k - 1) as f64 * (2 * k) as f64);
        sum += term;
    }
    -sum
}

/// Stable, overflow-safe logistic with correctly rounded exp.
#[inline]
pub fn sigmoid(z: f64) -> f64 {
    if z >= 0.0 {
        1.0 / (1.0 + bf_core::jsmath::exp(-z))
    } else {
        let e = bf_core::jsmath::exp(z);
        e / (1.0 + e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_items_follow_buys_and_sells() {
        let mut c = ItemClasses::default();
        c.completed.extend([3078, 3074, 3053]);
        c.boots.insert(3047);
        let p = vec![(1055, 10), (3047, 400), (3078, 600), (3074, 900), (-3078, 1200), (3053, 1300)];
        assert_eq!(owned_at(&p, 700, &c), (vec![3078], Some(3047)));
        assert_eq!(owned_at(&p, 1250, &c), (vec![3074], Some(3047)));
        assert_eq!(owned_at(&p, 2000, &c), (vec![3074, 3053], Some(3047)));
    }

    #[test]
    fn patch_age_counts_patches_seen() {
        let mut c = PatchClock::default();
        for v in ["16.22.1.1", "16.24.3.1", "17.1.1.1", "17.1.5.1"] {
            c.observe(GameVersion::parse(v).unwrap());
        }
        assert_eq!(c.age(Patch { major: 16, minor: 24 }), 1);
        let o = WeightOptions::default();
        assert_eq!(c.weight(GameVersion::parse("17.1.5.1"), &o), 1.0);
        assert_eq!(c.weight(GameVersion::parse("17.1.1.1"), &o), o.old_hotfix);
        assert!((c.weight(GameVersion::parse("16.22.1.1"), &o) - o.patch_decay * o.patch_decay).abs() < 1e-15);
    }

    #[test]
    fn rng_is_stable_and_normal_is_sane() {
        let mut r = Rng(42);
        let a: Vec<u64> = (0..3).map(|_| r.next_u64()).collect();
        let mut r2 = Rng(42);
        assert_eq!(a, (0..3).map(|_| r2.next_u64()).collect::<Vec<_>>());
        let n: Vec<f64> = (0..20000).map(|_| r.normal()).collect();
        let mean = n.iter().sum::<f64>() / n.len() as f64;
        let var = n.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n.len() as f64;
        assert!(mean.abs() < 0.03 && (var - 1.0).abs() < 0.05, "{mean} {var}");
        assert!((cos_2pi(0.0) - 1.0).abs() < 1e-14 && (cos_2pi(0.5) + 1.0).abs() < 1e-14);
    }
}
