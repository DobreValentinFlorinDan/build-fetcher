//! Synthetic matches with known, planted effects, and an in-memory `DataSource` over them. The learner's tests use
//! it to check that what was planted is recovered; the integrator can use it to measure the whole pipeline.
//!
//! Each match: 10 champions from per-role pools, a build plan per player (rune page, summoners, skill max, starting
//! items, items in order, boots) from per-role menus that respect item exclusivity, and a winner drawn from
//! `sigmoid(Σ team effects)` with logistic noise. The game state (gold, CS and level per minute, kills, towers,
//! dragons, barons) drifts toward the winner, so it is informative and more so later in the game. Planted effects:
//!
//! - `RUNE_VS_OPPONENT`: Irelia TOP with Conqueror against Darius gains +0.45 logit (≈ +11 percentage points at 50%);
//!   Grasp against Darius, and Conqueror against anyone else, gain nothing.
//! - `ITEM_VS_MAGIC`: a top or jungle player who builds Maw of Malmortius gains +0.6 logit when the enemy team is
//!   magic-heavy (`DamageProfile::Magic`) and loses 0.1 otherwise.
//! - `FIRST_ITEM`: Irelia rushing Blade of the Ruined King gains +0.3 over Trinity Force.
//! - `ALLY_SYNERGY`: Jinx (bottom) with Lulu on her team gains +0.5 logit. Step 1 has no champion × ally features,
//!   so only step 2 (the shared network) can learn this one.
//! - `ITEM_VS_ARMOR` (only with `SyntheticConfig::armor_effect`, so the other sets stay as they were): a bottom
//!   player who builds Lord Dominik's Regards gains +0.6 logit when the enemy team builds 7 or more armor items and
//!   boots, and loses 0.1 otherwise. Only the enemy's items tell the two apart (1.2.0).
//! - Thin champion: Ivern (jungle) is picked rarely (a few dozen games per 4,000 matches).
//!
//! Everything else (other runes, summoners, skill orders, items) has no effect, apart from a small fixed strength per
//! champion.

use crate::features::{sigmoid, ItemClasses, Rng};
use bf_core::build::*;
use bf_core::ddragon::StaticData;
use bf_core::engine::exclusivity::{build_exclusivity_index, ExclusivityIndex};
use std::collections::{BTreeMap, HashMap};

pub const IRELIA: i64 = 39;
pub const DARIUS: i64 = 122;
pub const IVERN: i64 = 427;
pub const CONQUEROR: i64 = 8010;
pub const GRASP: i64 = 8437;
pub const MAW: i64 = 3156;
pub const BOTRK: i64 = 3153;
pub const TRINITY: i64 = 3078;

pub const RUNE_VS_OPPONENT: f64 = 0.45;
pub const ITEM_VS_MAGIC: f64 = 0.6;
pub const ITEM_NOT_VS_MAGIC: f64 = -0.1;
pub const FIRST_ITEM: f64 = 0.3;
pub const ALLY_SYNERGY: f64 = 0.5;
pub const LDR: i64 = 3036;
pub const ITEM_VS_ARMOR: f64 = 0.6;
pub const ITEM_NOT_VS_ARMOR: f64 = -0.1;
/// Enemy armor items and boots from which `ITEM_VS_ARMOR` applies.
pub const ARMOR_STACK: usize = 7;
pub const JINX: i64 = 222;
pub const LULU: i64 = 117;

#[derive(Clone, Debug)]
pub struct SyntheticConfig {
    /// Training records (returned by `records_after`).
    pub matches: usize,
    /// Held-out records (`validation_records`).
    pub validation: usize,
    pub seed: u64,
    /// Game versions, oldest first; matches are spread over them in order.
    pub versions: Vec<&'static str>,
    /// Share of training records that keep their timeline (the rest are draft-only, like the store's tier B).
    pub timeline_share: f64,
    /// Share of matches where the user (is_me) plays Irelia TOP.
    pub me_share: f64,
    /// Pick weight of the thin champion among junglers (others weigh about 1 in total).
    pub thin_weight: f64,
    /// Every winner inverted (a poisoned batch for the guardrail tests).
    pub flip_labels: bool,
    /// Multiplies every planted effect (0: no effects).
    pub effect_scale: f64,
    /// First match id number (to append sources without clashes).
    pub first_id: u64,
    /// Plant `ITEM_VS_ARMOR`.
    pub armor_effect: bool,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        SyntheticConfig {
            matches: 4000,
            validation: 800,
            seed: 1,
            versions: vec!["16.20.1.1", "16.21.1.1", "16.22.1.1", "16.22.4.1"],
            timeline_share: 0.9,
            me_share: 0.03,
            thin_weight: 0.004,
            flip_labels: false,
            effect_scale: 1.0,
            first_id: 7_000_000_000,
            armor_effect: false,
        }
    }
}

/// A rune page: style, perks, sub style, sub perks, shards.
type Page = (i64, [i64; 4], i64, [i64; 2], [i64; 3]);

struct Menu {
    pages: Vec<Page>,
    summoners: Vec<[i64; 2]>,
    skills: Vec<[u8; 3]>,
    starting: Vec<Vec<i64>>,
    first: Vec<i64>,
    core: Vec<i64>,
    late: Vec<i64>,
    boots: Vec<i64>,
}

const SHARDS: [i64; 3] = [5008, 5008, 5011];

fn menus(classes: &ItemClasses, data: &StaticData) -> [Menu; 5] {
    let done = |v: &[i64]| v.iter().copied().filter(|i| classes.completed.contains(i)).collect::<Vec<_>>();
    let boots = |v: &[i64]| v.iter().copied().filter(|i| classes.boots.contains(i)).collect::<Vec<_>>();
    let start = |v: Vec<Vec<i64>>| v.into_iter().filter(|s| s.iter().all(|i| data.item(*i).is_some())).collect::<Vec<_>>();
    let conq: Page = (8000, [8010, 9111, 9104, 8299], 8400, [8444, 8473], SHARDS);
    let grasp: Page = (8400, [8437, 8446, 8444, 8451], 8000, [9111, 9104], SHARDS);
    let elec: Page = (8100, [8112, 8143, 8140, 8135], 8000, [9111, 9104], SHARDS);
    let fighter_core = [3053, 3071, 6333, 3074, 6610, 3161, 3181];
    let fighter_late = [MAW, 6333, 3026, 3065, 3075, 3742, 3053];
    let top = Menu {
        pages: vec![conq, grasp],
        summoners: vec![[4, 12], [4, 14]],
        skills: vec![[1, 3, 2], [3, 1, 2]],
        starting: start(vec![vec![1055, 2003], vec![1054, 2003]]),
        first: done(&[BOTRK, TRINITY, 3074, 3071, 6610]),
        core: done(&fighter_core),
        late: done(&fighter_late),
        boots: boots(&[3047, 3111]),
    };
    let jungle = Menu {
        pages: vec![conq, elec],
        summoners: vec![[4, 11]],
        skills: vec![[1, 3, 2], [3, 1, 2]],
        starting: start(vec![vec![1101, 2003], vec![1102, 2003], vec![1103, 2003]]),
        first: done(&[3071, 6610, 3074, 3161, TRINITY]),
        core: done(&fighter_core),
        late: done(&fighter_late),
        boots: boots(&[3047, 3111, 3006]),
    };
    let middle = Menu {
        pages: vec![(8100, [8112, 8139, 8140, 8135], 8200, [8226, 8237], SHARDS), (8200, [8229, 8226, 8210, 8237], 8300, [8345, 8347], SHARDS)],
        summoners: vec![[4, 14], [4, 12]],
        skills: vec![[1, 2, 3], [1, 3, 2]],
        starting: start(vec![vec![1056, 2003, 2003]]),
        first: done(&[6655, 4645, 3165]),
        core: done(&[3089, 3157, 3135, 3165, 4645, 6653, 3102]),
        late: done(&[3089, 3157, 3135, 3102, 3116]),
        boots: boots(&[3020, 3158]),
    };
    let bottom = Menu {
        pages: vec![(8000, [8008, 9111, 9104, 8014], 8300, [8345, 8347], SHARDS), (8000, [8005, 9111, 9103, 8014], 8100, [8139, 8135], SHARDS)],
        summoners: vec![[4, 7], [4, 21]],
        skills: vec![[1, 2, 3], [1, 3, 2]],
        starting: start(vec![vec![1055, 2003]]),
        first: done(&[6672, 3153, 3031]),
        core: done(&[3031, 3094, 3036, 3046, 3033, 6672]),
        late: done(&[3036, 3033, 3026, 3046, 3094]),
        boots: boots(&[3006]),
    };
    let utility = Menu {
        pages: vec![(8400, [8439, 8446, 8429, 8451], 8300, [8345, 8347], SHARDS), (8200, [8214, 8226, 8210, 8237], 8400, [8463, 8453], SHARDS)],
        summoners: vec![[4, 14], [3, 4]],
        skills: vec![[1, 3, 2], [3, 1, 2]],
        starting: start(vec![vec![3865, 2003]]),
        first: done(&[3190, 3050, 6617, 2065]),
        core: done(&[3190, 3109, 3050, 2065, 3107, 6617, 3504, 3222]),
        late: done(&[3109, 3107, 3222, 3075, 3065]),
        boots: boots(&[3111, 3158, 3047]),
    };
    [top, jungle, middle, bottom, utility]
}

fn pools(thin_weight: f64) -> [Vec<(i64, f64)>; 5] {
    // Enough AP picks (Vladimir, Mordekaiser, Lillia, Karthus, Ziggs, Brand, Morgana...) that a magic-heavy enemy team
    // (DamageProfile::Magic) comes up in about one game in ten.
    [
        vec![(IRELIA, 0.28), (DARIUS, 0.28), (86, 0.06), (82, 0.12), (8, 0.12), (10, 0.07)],
        vec![(64, 0.2), (254, 0.15), (60, 0.15), (131, 0.15), (876, 0.15), (30, 0.1), (IVERN, thin_weight)],
        vec![(103, 0.25), (134, 0.25), (112, 0.2), (1, 0.15), (238, 0.15)],
        vec![(222, 0.3), (51, 0.25), (81, 0.15), (115, 0.2), (145, 0.1)],
        vec![(412, 0.15), (117, 0.25), (89, 0.15), (267, 0.15), (63, 0.15), (25, 0.15)],
    ]
}

fn pick_weighted(rng: &mut Rng, pool: &[(i64, f64)], taken: &[i64]) -> i64 {
    let total: f64 = pool.iter().filter(|(c, _)| !taken.contains(c)).map(|(_, w)| w).sum();
    let mut x = rng.uniform() * total;
    for (c, w) in pool {
        if taken.contains(c) {
            continue;
        }
        if x < *w {
            return *c;
        }
        x -= w;
    }
    pool.iter().rev().find(|(c, _)| !taken.contains(c)).unwrap().0
}

fn pick<T: Clone>(rng: &mut Rng, v: &[T]) -> T {
    v[rng.below(v.len())].clone()
}

/// Skill-ups for a max order (a, b, c): the first three levels one each, R at 6/11/16, the rest by priority.
fn skill_order(m: [u8; 3]) -> Vec<u8> {
    let [a, b, c] = m;
    vec![a, b, c, a, a, 4, a, b, a, b, 4, b, b, c, c, 4, c, c]
}

struct Plan {
    page: Page,
    summoners: [i64; 2],
    skills: [u8; 3],
    starting: Vec<i64>,
    items: Vec<i64>,
    boots: i64,
}

fn plan(rng: &mut Rng, menu: &Menu, champion: i64, excl: &ExclusivityIndex) -> Plan {
    let page = pick(rng, &menu.pages);
    let mut items: Vec<i64> = vec![];
    let first = if champion == IRELIA { if rng.uniform() < 0.5 { BOTRK } else { TRINITY } } else { pick(rng, &menu.first) };
    items.push(first);
    let add_from = |rng: &mut Rng, pool: &[i64], items: &mut Vec<i64>, n: usize| {
        let mut tries = 0;
        while items.len() < n && tries < 50 {
            tries += 1;
            let c = pick(rng, pool);
            if excl.blocker(items, c).is_none() {
                items.push(c);
            }
        }
    };
    add_from(rng, &menu.core, &mut items, 3);
    // Maw is picked 30% of the time as the 4th item whatever the enemy, so its effect can be told apart.
    if menu.late.contains(&MAW) && rng.uniform() < 0.3 && excl.blocker(&items, MAW).is_none() {
        items.push(MAW);
    }
    let rest: Vec<i64> = menu.late.iter().copied().filter(|i| *i != MAW).collect();
    add_from(rng, &rest, &mut items, 6);
    Plan { page, summoners: pick(rng, &menu.summoners), skills: pick(rng, &menu.skills), starting: pick(rng, &menu.starting), items, boots: pick(rng, &menu.boots) }
}

/// The in-memory store: training records with sequence numbers, a held-out set, tier B counts and volumes.
pub struct SyntheticSource {
    pub records: Vec<(u64, MatchRecord)>,
    pub validation: Vec<MatchRecord>,
    pub counts: HashMap<(i64, Role), Vec<OptionCounts>>,
    pub volumes: Vec<PairVolume>,
    pub current: Option<Patch>,
}

struct Generator<'a> {
    data: &'a StaticData,
    classes: ItemClasses,
    excl: ExclusivityIndex,
    menus: [Menu; 5],
    pools: [Vec<(i64, f64)>; 5],
    strength: BTreeMap<i64, f64>,
    cfg: SyntheticConfig,
}

impl Generator<'_> {
    fn effect(&self, champion: i64, role: Role, plan: &Plan, opponent: i64, enemy_profile: Option<DamageProfile>, allies: &[i64], enemy_armor: usize) -> f64 {
        let s = self.cfg.effect_scale;
        let mut e = self.strength.get(&champion).copied().unwrap_or(0.0);
        if self.cfg.armor_effect && role == Role::Bottom && plan.items.contains(&LDR) {
            e += if enemy_armor >= ARMOR_STACK { ITEM_VS_ARMOR } else { ITEM_NOT_VS_ARMOR } * s;
        }
        if champion == IRELIA && role == Role::Top && opponent == DARIUS && plan.page.1[0] == CONQUEROR {
            e += RUNE_VS_OPPONENT * s;
        }
        if matches!(role, Role::Top | Role::Jungle) && plan.items.contains(&MAW) {
            e += if enemy_profile == Some(DamageProfile::Magic) { ITEM_VS_MAGIC } else { ITEM_NOT_VS_MAGIC } * s;
        }
        if champion == IRELIA && plan.items.first() == Some(&BOTRK) {
            e += FIRST_ITEM * s;
        }
        if champion == JINX && allies.contains(&LULU) {
            e += ALLY_SYNERGY * s;
        }
        e
    }

    fn one(&self, rng: &mut Rng, n: u64, version: GameVersion, start_ms: i64, keep_timeline: bool) -> MatchRecord {
        let mut champs = [[0i64; 5]; 2];
        let mut taken = vec![];
        let me = rng.uniform() < self.cfg.me_share;
        for (t, team) in champs.iter_mut().enumerate() {
            for (r, slot) in team.iter_mut().enumerate() {
                let c = if me && t == 0 && r == 0 { IRELIA } else { pick_weighted(rng, &self.pools[r], &taken) };
                *slot = c;
                taken.push(c);
            }
        }
        // The user's Irelia must not also be on the other team.
        if me && champs[1][0] == IRELIA {
            champs[1][0] = DARIUS;
        }
        let plans: Vec<Vec<Plan>> = (0..2).map(|t| (0..5).map(|r| plan(rng, &self.menus[r], champs[t][r], &self.excl)).collect()).collect();
        let profile = |t: usize| DamageProfile::of_team(self.data, &champs[1 - t]);
        // Armor items and boots each team ends up with (what `ITEM_VS_ARMOR` responds to on the other side).
        let armor = |t: usize| -> usize {
            plans[t].iter().flat_map(|p| p.items.iter().chain(std::iter::once(&p.boots))).filter(|i| self.classes.tags.get(i).map(|m| m & 1 != 0).unwrap_or(false)).count()
        };
        let mut z = 0.0;
        for t in 0..2 {
            for (r, role) in Role::ALL.iter().enumerate() {
                let e = self.effect(champs[t][r], *role, &plans[t][r], champs[1 - t][r], profile(t), &champs[t], armor(1 - t));
                z += if t == 0 { e } else { -e };
            }
        }
        let u = rng.uniform().clamp(1e-12, 1.0 - 1e-12);
        let noise = bf_core::jsmath::log(u / (1.0 - u));
        let a_end = z + noise;
        let win100 = a_end > 0.0;
        let duration_min = (24.0 + 18.0 * rng.uniform() - 4.0 * a_end.abs().min(3.0) / 3.0).max(20.0);
        let duration_s = (duration_min * 60.0) as u32;
        let minutes = duration_s / 60;
        // Advantage path toward the final advantage, with a random walk on top.
        let mut adv = vec![0.0; minutes as usize + 1];
        let mut walk = 0.0;
        for (m, a) in adv.iter_mut().enumerate() {
            walk += 0.15 * rng.normal();
            *a = a_end * (m as f64 / duration_min).min(1.0) + walk * (m as f64 / duration_min).min(1.0);
        }
        let lane_noise: Vec<Vec<f64>> = (0..5).map(|_| (0..=minutes).map(|_| rng.normal()).collect()).collect();
        let mut events = vec![];
        for m in 1..=minutes {
            let a = adv[m as usize];
            let p100 = sigmoid(1.2 * a);
            for _ in 0..3 {
                if rng.uniform() < 0.22 {
                    events.push(TeamEvent { second: m * 60 - rng.below(59) as u32, team: if rng.uniform() < p100 { 100 } else { 200 }, kind: TeamEventKind::Kill });
                }
            }
            if m >= 12 && rng.uniform() < 0.25 {
                events.push(TeamEvent { second: m * 60 - 30, team: if rng.uniform() < sigmoid(1.5 * a) { 100 } else { 200 }, kind: TeamEventKind::Tower });
            }
            if m >= 6 && (m - 6) % 5 == 0 {
                events.push(TeamEvent { second: m * 60 - 10, team: if rng.uniform() < p100 { 100 } else { 200 }, kind: TeamEventKind::Dragon });
            }
            if m >= 25 && (m - 25) % 7 == 0 && rng.uniform() < 0.6 {
                events.push(TeamEvent { second: m * 60 - 20, team: if rng.uniform() < sigmoid(1.5 * a) { 100 } else { 200 }, kind: TeamEventKind::Baron });
            }
        }
        events.sort_by_key(|e| e.second);
        let mut players = vec![];
        let mut frames = vec![];
        for t in 0..2 {
            for (r, role) in Role::ALL.iter().enumerate() {
                let p = &plans[t][r];
                let side = if t == 0 { 1.0 } else { -1.0 };
                let mut f = vec![];
                for m in 0..=minutes {
                    let lane = 120.0 * adv[m as usize] + 250.0 * (m as f64 / 10.0).sqrt() * lane_noise[r][m as usize];
                    let gold = (500.0 + 400.0 * m as f64 + side * lane / 2.0).max(500.0);
                    let cs = (7.5 * m as f64 + side * lane / 40.0).max(0.0);
                    let level = ((1.0 + m as f64 * 0.62 + side * lane / 1500.0).round()).clamp(1.0, 18.0);
                    f.push(Frame { minute: m as u16, total_gold: gold as u32, cs: cs as u16, level: level as u8, xp: (m * 600) as u32 });
                }
                let mut purchases: Vec<(i64, u32)> = p.starting.iter().map(|i| (*i, 15)).collect();
                purchases.push((p.boots, 480 + rng.below(120) as u32));
                let slots = [600, 960, 1320, 1560, 1860, 2160];
                for (k, it) in p.items.iter().enumerate() {
                    purchases.push((*it, slots[k] + rng.below(170) as u32));
                }
                purchases.retain(|(_, s)| *s < duration_s);
                purchases.sort_by_key(|x| x.1);
                let order = skill_order(p.skills);
                let levels = f.last().map(|x| x.level as usize).unwrap_or(18);
                let won = win100 == (t == 0);
                let won = if self.cfg.flip_labels { !won } else { won };
                let mut final_items = [0i64; 7];
                let owned: Vec<i64> = purchases.iter().filter(|(i, _)| self.classes.completed.contains(i) || self.classes.boots.contains(i)).map(|x| x.0).collect();
                for (k, it) in owned.iter().take(6).enumerate() {
                    final_items[k] = *it;
                }
                final_items[6] = 3340;
                let magic = DamageProfile::magic_share_of(self.data, champs[t][r]).unwrap_or(0.5);
                let dmg = 15_000.0 + 600.0 * duration_min;
                players.push(PlayerRecord {
                    champion: champs[t][r],
                    role: Some(*role),
                    team: if t == 0 { 100 } else { 200 },
                    win: won,
                    is_me: me && t == 0 && r == 0,
                    summoners: p.summoners,
                    rune_style: p.page.0,
                    perks: p.page.1,
                    rune_sub_style: p.page.2,
                    sub_perks: p.page.3,
                    shards: p.page.4,
                    final_items,
                    kills: rng.below(10) as u16,
                    deaths: rng.below(8) as u16,
                    assists: rng.below(12) as u16,
                    gold_earned: f.last().map(|x| x.total_gold).unwrap_or(0),
                    cs: f.last().map(|x| x.cs).unwrap_or(0),
                    champ_level: f.last().map(|x| x.level).unwrap_or(1),
                    physical_damage_to_champions: (dmg * (1.0 - magic)) as u32,
                    magic_damage_to_champions: (dmg * magic) as u32,
                    true_damage_to_champions: 500,
                    purchases: if keep_timeline { purchases } else { vec![] },
                    skill_order: if keep_timeline { order[..levels.min(18)].to_vec() } else { vec![] },
                });
                frames.push(f);
            }
        }
        if !keep_timeline {
            frames.clear();
            events.clear();
        }
        let platform = Platform::ALL[(n % 3) as usize];
        MatchRecord {
            match_id: format!("{}_{}", platform.as_str(), self.cfg.first_id + n),
            platform: Some(platform),
            version: Some(version),
            rank: Some(if rng.uniform() < 0.75 { RankBand::MasterPlus } else { RankBand::Diamond }),
            start_ms,
            duration_s,
            players,
            frames,
            events,
        }
    }
}

/// Generates training and validation records with the planted effects above, and the counts the store would keep.
pub fn generate(data: &StaticData, cfg: &SyntheticConfig) -> SyntheticSource {
    let classes = ItemClasses::new(data);
    let excl = build_exclusivity_index(data);
    let menus = menus(&classes, data);
    let mut srng = Rng(0x57_2e46);
    let mut strength = BTreeMap::new();
    for pool in pools(cfg.thin_weight).iter() {
        for (c, _) in pool {
            // Planted champions stay at 0 so their planted effects are the only thing that moves them.
            let s = if [IRELIA, DARIUS, IVERN, JINX, LULU].contains(c) { 0.0 } else { 0.12 * srng.normal() };
            strength.insert(*c, s);
        }
    }
    let g = Generator { data, classes, excl, menus, pools: pools(cfg.thin_weight), strength, cfg: cfg.clone() };
    let versions: Vec<GameVersion> = cfg.versions.iter().map(|v| GameVersion::parse(v).unwrap()).collect();
    let mut rng = Rng(cfg.seed.wrapping_mul(0x2545_f491_4f6c_dd1d) ^ 0x1234);
    let t0 = bf_core::testkit::date_utc(2026, 9, 1);
    let mut records = vec![];
    for n in 0..cfg.matches {
        let v = versions[(n * versions.len() / cfg.matches.max(1)).min(versions.len() - 1)];
        let keep = rng.uniform() < cfg.timeline_share;
        records.push((n as u64 + 1, g.one(&mut rng, n as u64, v, t0 + n as i64 * 60_000, keep)));
    }
    let mut vrng = Rng(cfg.seed ^ 0x7a11_da7e);
    let validation = (0..cfg.validation)
        .map(|n| {
            let v = versions[(n * versions.len() / cfg.validation.max(1)).min(versions.len() - 1)];
            g.one(&mut vrng, 1_000_000 + n as u64, v, t0 + n as i64 * 60_000, true)
        })
        .collect();
    let mut src = SyntheticSource { records: vec![], validation, counts: HashMap::new(), volumes: vec![], current: versions.last().map(|v| v.patch) };
    src.add_records(records, &g.classes, data);
    src
}

impl SyntheticSource {
    /// Appends records (renumbered after the last one) and recounts. Validation stays as it is.
    pub fn append(&mut self, other: SyntheticSource, data: &StaticData) {
        let classes = ItemClasses::new(data);
        let base = self.records.last().map(|r| r.0).unwrap_or(0);
        let more: Vec<(u64, MatchRecord)> = other.records.into_iter().enumerate().map(|(i, (_, r))| (base + i as u64 + 1, r)).collect();
        let mut all = std::mem::take(&mut self.records);
        all.extend(more);
        self.counts.clear();
        self.add_records(all, &classes, data);
        if other.current > self.current {
            self.current = other.current;
        }
    }

    fn add_records(&mut self, records: Vec<(u64, MatchRecord)>, classes: &ItemClasses, data: &StaticData) {
        // Pass 1: each champion-role's own win rate, the counts' baseline (expected_wins) before a model exists.
        let mut rate: BTreeMap<(i64, Role), (f64, f64)> = BTreeMap::new();
        let mut vols: BTreeMap<(i64, Role, Patch), PairVolume> = BTreeMap::new();
        for (_, r) in &records {
            let patch = r.version.map(|v| v.patch).unwrap_or(Patch { major: 0, minor: 0 });
            for p in &r.players {
                let Some(role) = p.role else { continue };
                let e = rate.entry((p.champion, role)).or_insert((0.0, 0.0));
                e.0 += p.win as u8 as f64;
                e.1 += 1.0;
                let v = vols.entry((p.champion, role, patch)).or_insert(PairVolume { champion: p.champion, role, patch, games: 0, wins: 0, newest_ms: 0 });
                v.games += 1;
                v.wins += p.win as u32;
                v.newest_ms = v.newest_ms.max(r.start_ms);
            }
        }
        type Key = (i64, Role, Patch, RankBand, CountContext, BuildOption);
        let mut agg: BTreeMap<Key, (u32, u32, f64)> = BTreeMap::new();
        for (_, r) in &records {
            let patch = r.version.map(|v| v.patch).unwrap_or(Patch { major: 0, minor: 0 });
            for (i, p) in r.players.iter().enumerate() {
                let Some(role) = p.role else { continue };
                let (w, g) = rate[&(p.champion, role)];
                let expected = w / g;
                let rank = if p.is_me { RankBand::Mine } else { r.rank.unwrap_or(RankBand::MasterPlus) };
                let enemies: Vec<i64> = r.players.iter().filter(|q| q.team != p.team).map(|q| q.champion).collect();
                let ctxs = [
                    CountContext::default(),
                    CountContext { lane_opponent: r.lane_opponent(i).map(|o| o.champion), enemy_profile: None },
                    CountContext { lane_opponent: None, enemy_profile: DamageProfile::of_team(data, &enemies) },
                ];
                for opt in crate::features::options_of(r, i, classes) {
                    for c in ctxs.iter() {
                        if *c != CountContext::default() && c.lane_opponent.is_none() && c.enemy_profile.is_none() {
                            continue;
                        }
                        let e = agg.entry((p.champion, role, patch, rank, *c, opt.clone())).or_insert((0, 0, 0.0));
                        e.0 += 1;
                        e.1 += p.win as u32;
                        e.2 += expected;
                    }
                }
            }
        }
        for ((c, role, patch, rank, context, option), (games, wins, expected_wins)) in agg {
            self.counts.entry((c, role)).or_default().push(OptionCounts { patch, rank, context, option, games, wins, expected_wins });
        }
        self.volumes = vols.into_values().collect();
        self.records = records;
    }
}

impl DataSource for SyntheticSource {
    fn counts(&self, q: &CountQuery) -> anyhow::Result<Vec<OptionCounts>> {
        Ok(self
            .counts
            .get(&(q.champion, q.role))
            .map(|v| v.iter().filter(|c| (q.patches.is_empty() || q.patches.contains(&c.patch)) && (q.kinds.is_empty() || q.kinds.contains(&c.option.kind))).cloned().collect())
            .unwrap_or_default())
    }
    fn volumes(&self, patches: &[Patch]) -> anyhow::Result<Vec<PairVolume>> {
        Ok(self.volumes.iter().filter(|v| patches.is_empty() || patches.contains(&v.patch)).cloned().collect())
    }
    fn timeline_count(&self) -> anyhow::Result<u64> {
        Ok(self.records.iter().filter(|r| r.1.has_timeline()).count() as u64)
    }
    fn records_after(&self, after_seq: u64, limit: usize) -> anyhow::Result<Vec<(u64, MatchRecord)>> {
        let at = self.records.partition_point(|r| r.0 <= after_seq);
        Ok(self.records[at..(at + limit).min(self.records.len())].to_vec())
    }
    fn validation_records(&self) -> anyhow::Result<Vec<MatchRecord>> {
        Ok(self.validation.clone())
    }
    fn current_patch(&self) -> anyhow::Result<Option<Patch>> {
        Ok(self.current)
    }
}
