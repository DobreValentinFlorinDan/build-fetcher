//! Port of `server/model/profiles.ts`: champion profiles (B1) from every stored player and the kept totals.

use crate::db::ChampionTotals;
use crate::ddragon::{ChampionMeta, StaticData};
use indexmap::IndexMap;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct ChampionProfile {
    pub champion_id: i64,
    pub position: Option<String>,
    pub games: f64,
    pub cls: String,
    pub range: f64,
    pub ranged: bool,
    pub phys_share: f64,
    pub magic_share: f64,
    pub true_share: f64,
    pub dmg_per_min: f64,
    pub heal_per_min: f64,
    pub ally_heal_per_min: f64,
    pub shield_per_min: f64,
    pub cc_per_min: f64,
    pub tank_per_min: f64,
}

pub const RANGED_FROM: f64 = 300.0;

#[derive(Clone, Copy, Debug)]
pub struct ProfileOptions {
    pub k: f64,
    pub older_weight: f64,
}
pub const PROFILE_DEFAULTS: ProfileOptions = ProfileOptions { k: 50.0, older_weight: 0.5 };

#[derive(Clone, Copy, Debug, Default)]
struct Sums {
    games: f64,
    minutes: f64,
    phys: f64,
    magic: f64,
    true_: f64,
    taken: f64,
    heal: f64,
    heal_allies: f64,
    shield: f64,
    cc: f64,
}

fn add_to(s: &mut Sums, t: &ChampionTotals, w: f64) {
    s.games += t.games * w;
    s.minutes += (t.seconds / 60.0) * w;
    s.phys += t.dmg_physical * w;
    s.magic += t.dmg_magic * w;
    s.true_ += t.dmg_true * w;
    s.taken += (t.dmg_taken + t.dmg_mitigated) * w;
    s.heal += super::logistic::js_max(0.0, t.heal - t.heal_allies) * w;
    s.heal_allies += t.heal_allies * w;
    s.shield += t.shield_allies * w;
    s.cc += t.cc_time * w;
}

fn merge(a: &mut Sums, b: &Sums) {
    a.games += b.games;
    a.minutes += b.minutes;
    a.phys += b.phys;
    a.magic += b.magic;
    a.true_ += b.true_;
    a.taken += b.taken;
    a.heal += b.heal;
    a.heal_allies += b.heal_allies;
    a.shield += b.shield;
    a.cc += b.cc;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    pub phys_share: f64,
    pub magic_share: f64,
    pub true_share: f64,
    pub dmg_per_min: f64,
    pub heal_per_min: f64,
    pub ally_heal_per_min: f64,
    pub shield_per_min: f64,
    pub cc_per_min: f64,
    pub tank_per_min: f64,
}

fn stats_of(s: &Sums) -> Stats {
    let dmg = s.phys + s.magic + s.true_;
    let per = |x: f64| if s.minutes > 0.0 { x / s.minutes } else { 0.0 };
    Stats {
        phys_share: if dmg > 0.0 { s.phys / dmg } else { 0.0 },
        magic_share: if dmg > 0.0 { s.magic / dmg } else { 0.0 },
        true_share: if dmg > 0.0 { s.true_ / dmg } else { 0.0 },
        dmg_per_min: per(dmg),
        heal_per_min: per(s.heal),
        ally_heal_per_min: per(s.heal_allies),
        shield_per_min: per(s.shield),
        cc_per_min: per(s.cc),
        tank_per_min: per(s.taken),
    }
}

/// x̂ = (n·x̄ + k·prior) / (n + k), stat by stat.
fn shrink(own: &Stats, n: f64, prior: &Stats, k: f64) -> Stats {
    let f = |a: f64, b: f64| (n * a + k * b) / (n + k);
    Stats {
        phys_share: f(own.phys_share, prior.phys_share),
        magic_share: f(own.magic_share, prior.magic_share),
        true_share: f(own.true_share, prior.true_share),
        dmg_per_min: f(own.dmg_per_min, prior.dmg_per_min),
        heal_per_min: f(own.heal_per_min, prior.heal_per_min),
        ally_heal_per_min: f(own.ally_heal_per_min, prior.ally_heal_per_min),
        shield_per_min: f(own.shield_per_min, prior.shield_per_min),
        cc_per_min: f(own.cc_per_min, prior.cc_per_min),
        tank_per_min: f(own.tank_per_min, prior.tank_per_min),
    }
}

fn no_data_prior(c: Option<&ChampionMeta>) -> Stats {
    let attack = c.map(|c| c.info.attack).unwrap_or(5.0);
    let magic = c.map(|c| c.info.magic).unwrap_or(5.0);
    let magic_share = if attack + magic > 0.0 { magic / (attack + magic) } else { 0.5 };
    Stats {
        phys_share: 1.0 - magic_share,
        magic_share,
        true_share: 0.0,
        dmg_per_min: 700.0,
        heal_per_min: 120.0,
        ally_heal_per_min: 0.0,
        shield_per_min: 0.0,
        cc_per_min: 0.8,
        tank_per_min: 1500.0,
    }
}

struct ByChampion {
    sums: Sums,
    positions: HashMap<String, Sums>,
}

pub struct ChampionProfiles {
    by_champion: IndexMap<i64, ByChampion>,
    class_stats: HashMap<String, Stats>,
    global: Option<Stats>,
    cache: RwLock<HashMap<(i64, String), Arc<ChampionProfile>>>,
    healer_from: f64,
    tank_from: f64,
    pub games: f64,
    data: Arc<StaticData>,
    o: ProfileOptions,
}

impl ChampionProfiles {
    pub fn new(current: &[ChampionTotals], older: &[ChampionTotals], data: Arc<StaticData>, o: ProfileOptions) -> ChampionProfiles {
        let mut by_champion: IndexMap<i64, ByChampion> = IndexMap::new();
        let mut add = |t: &ChampionTotals, w: f64| {
            let c = by_champion.entry(t.champion_id).or_insert_with(|| ByChampion { sums: Sums::default(), positions: HashMap::new() });
            add_to(&mut c.sums, t, w);
            let pos = c.positions.entry(t.position.clone()).or_default();
            add_to(pos, t, w);
        };
        for t in current {
            add(t, 1.0);
        }
        for t in older {
            add(t, o.older_weight);
        }
        let cls_of = |id: i64| data.champions.get(&id).and_then(|c| c.tags.first().cloned()).unwrap_or_else(|| "Unknown".into());
        let mut classes: IndexMap<String, Sums> = IndexMap::new();
        let mut all = Sums::default();
        for (id, c) in &by_champion {
            let s = classes.entry(cls_of(*id)).or_default();
            merge(s, &c.sums);
            merge(&mut all, &c.sums);
        }
        let games = all.games / 10.0;
        let global = if all.games > 0.0 { Some(stats_of(&all)) } else { None };
        let mut class_stats = HashMap::new();
        for (cls, s) in &classes {
            let own = stats_of(s);
            class_stats.insert(cls.clone(), match &global { Some(g) => shrink(&own, s.games, g, o.k), None => own });
        }
        let mut p = ChampionProfiles {
            by_champion,
            class_stats,
            global,
            cache: RwLock::new(HashMap::new()),
            healer_from: f64::INFINITY,
            tank_from: f64::INFINITY,
            games,
            data,
            o,
        };
        let ids: Vec<i64> = p.by_champion.keys().copied().collect();
        let solid: Vec<Arc<ChampionProfile>> = ids.iter().map(|id| p.get(*id, None)).filter(|x| x.games >= 20.0).collect();
        if solid.len() >= 10 {
            let mut heals: Vec<f64> = solid.iter().map(|x| x.heal_per_min + x.ally_heal_per_min).collect();
            heals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mut tanks: Vec<f64> = solid.iter().map(|x| x.tank_per_min).collect();
            tanks.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            p.healer_from = 2.0 * heals[heals.len() / 2];
            p.tank_from = tanks[(tanks.len() as f64 * 0.8).floor() as usize];
        }
        p
    }

    fn cls_of(&self, id: i64) -> String {
        self.data.champions.get(&id).and_then(|c| c.tags.first().cloned()).unwrap_or_else(|| "Unknown".into())
    }

    /// The profile of a champion, in a position when one is given (shrunk toward the champion's pooled profile).
    pub fn get(&self, champion_id: i64, position: Option<&str>) -> Arc<ChampionProfile> {
        let position = position.filter(|p| !p.is_empty());
        let key = (champion_id, position.unwrap_or("").to_string());
        if let Some(hit) = self.cache.read().get(&key) {
            return hit.clone();
        }
        let meta = self.data.champions.get(&champion_id);
        let cls = self.cls_of(champion_id);
        let prior = self.class_stats.get(&cls).copied().or(self.global).unwrap_or_else(|| no_data_prior(meta));
        let c = self.by_champion.get(&champion_id);
        let mut stats = match c {
            Some(c) => shrink(&stats_of(&c.sums), c.sums.games, &prior, self.o.k),
            None => prior,
        };
        let mut games = c.map(|c| c.sums.games).unwrap_or(0.0);
        if let Some(pos) = position.and_then(|p| c.and_then(|c| c.positions.get(p))) {
            stats = shrink(&stats_of(pos), pos.games, &stats, self.o.k);
            games = pos.games;
        }
        let range = meta.map(|m| m.attack_range).unwrap_or(0.0);
        let profile = Arc::new(ChampionProfile {
            champion_id,
            position: position.map(String::from),
            games,
            cls,
            range,
            ranged: range > RANGED_FROM,
            phys_share: stats.phys_share,
            magic_share: stats.magic_share,
            true_share: stats.true_share,
            dmg_per_min: stats.dmg_per_min,
            heal_per_min: stats.heal_per_min,
            ally_heal_per_min: stats.ally_heal_per_min,
            shield_per_min: stats.shield_per_min,
            cc_per_min: stats.cc_per_min,
            tank_per_min: stats.tank_per_min,
        });
        self.cache.write().insert(key, profile.clone());
        profile
    }

    pub fn is_healer(&self, p: &ChampionProfile) -> bool {
        p.heal_per_min + p.ally_heal_per_min > self.healer_from
    }
    pub fn is_tank(&self, p: &ChampionProfile) -> bool {
        p.tank_per_min > self.tank_from
    }
    pub fn calibrated(&self) -> bool {
        self.healer_from.is_finite()
    }
}

/// A team's damage mix: each champion's magic share weighted by the damage it deals (and by `threat` when given).
pub fn team_magic_share(team: &[Arc<ChampionProfile>], threat: Option<&[f64]>) -> f64 {
    let mut dmg = 0.0;
    let mut magic = 0.0;
    for (i, p) in team.iter().enumerate() {
        let w = p.dmg_per_min * threat.and_then(|t| t.get(i).copied()).unwrap_or(1.0);
        dmg += w;
        magic += w * p.magic_share;
    }
    if dmg > 0.0 {
        magic / dmg
    } else {
        0.5
    }
}
