//! League's combat math for one player at one minute (1.3.0): what the player's build adds up to, and what it does
//! against the enemy's. Everything comes from Data Dragon (item stat lines, champion base stats and growth) and from
//! what the player can see: every player's champion, level and items. Ability ratios are not in Data Dragon; each
//! champion's taste for each stat is learnt from games instead (the champion-role weights on these numbers).
//!
//! The vector has `COMBAT` numbers, each scaled to about 1:
//! - 0..20: the build's stats per 1,000 gold spent (`bf_core::ddragon::STAT_*` order). Per gold, so the numbers say
//!   what the build is made of rather than simply growing with game time (raw counts acted as a clock and left the
//!   first model overconfident late, on synthetic data in 1.2.0).
//! - 20..36: combat terms, listed at `TERM_NAMES`.

use bf_core::build::DamageProfile;
use bf_core::ddragon::*;

pub const COMBAT: usize = ITEM_STATS + TERMS;
pub const TERMS: usize = 16;
pub const TERM_NAMES: [&str; TERMS] = [
    "physical damage that gets through",
    "magic damage that gets through",
    "damage that gets through (your mix)",
    "your resistances against their damage mix",
    "your effective health against theirs",
    "crit with attack speed",
    "on-hit with attack speed",
    "ability haste on a caster",
    "your sustain against their anti-heal",
    "your anti-heal against their healing",
    "wasted crit chance",
    "magic resist against little magic damage",
    "armor against little physical damage",
    "penetration against low resistances",
    "stats off your damage type",
    "items with an active",
];

/// The scale of each stat: about one item's worth.
const UNIT: [f64; ITEM_STATS] = [50.0, 80.0, 0.5, 0.25, 0.3, 20.0, 0.3, 15.0, 0.3, 20.0, 400.0, 50.0, 50.0, 0.1, 0.1, 0.1, 25.0, 0.05, 300.0, 0.2];

/// One player as the combat math needs them: champion, level and owned items (completed items and boots).
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerIn<'a> {
    pub champion: i64,
    pub level: f64,
    pub items: &'a [i64],
}

struct Build {
    stats: [f64; ITEM_STATS],
    gold: f64,
    on_hit: f64,
    wounds: bool,
    healing: f64,
}

fn build(data: &StaticData, items: &[i64]) -> Build {
    let mut b = Build { stats: [0.0; ITEM_STATS], gold: 0.0, on_hit: 0.0, wounds: false, healing: 0.0 };
    for id in items {
        let Some(it) = data.item(*id) else { continue };
        for (k, v) in it.stats.iter().enumerate() {
            b.stats[k] += v;
        }
        b.gold += it.gold_total;
        if it.effects & EFFECT_ON_HIT != 0 {
            b.on_hit += 1.0;
        }
        if it.effects & EFFECT_WOUNDS != 0 {
            b.wounds = true;
        }
        if it.stats[STAT_LIFE_STEAL] > 0.0 || it.stats[STAT_OMNIVAMP] > 0.0 {
            b.healing += 1.0;
        }
    }
    b
}

fn actives(data: &StaticData, items: &[i64]) -> f64 {
    items.iter().filter(|i| data.item(**i).map(|it| it.effects & EFFECT_ACTIVE != 0).unwrap_or(false)).count() as f64
}

/// The share of a player's damage that is magic: half the champion's own, half the build's AP against AD.
fn magic_mix(data: &StaticData, champion: i64, s: &[f64; ITEM_STATS]) -> f64 {
    let own = DamageProfile::magic_share_of(data, champion).unwrap_or(0.5);
    let (ap, ad) = (s[STAT_AP] / UNIT[STAT_AP], s[STAT_AD] / UNIT[STAT_AD]);
    if ap + ad > 0.0 {
        0.5 * own + 0.5 * ap / (ap + ad)
    } else {
        own
    }
}

/// The combat vector of `me` against `enemies` (empty when the enemy is unknown: the terms that need it stay 0).
pub fn combat_vector(data: &StaticData, me: &PlayerIn, enemies: &[PlayerIn]) -> Vec<f32> {
    let mut v = vec![0.0f64; COMBAT];
    let b = build(data, me.items);
    let s = &b.stats;
    let per_k = b.gold.max(1000.0) / 1000.0;
    for k in 0..ITEM_STATS {
        v[k] = s[k] / UNIT[k] / per_k;
    }
    let base = data.champion(me.champion).map(|c| c.base).unwrap_or_default();
    let caster = data.champion(me.champion).map(|c| c.info.magic / 10.0).unwrap_or(0.5);
    let own_magic = DamageProfile::magic_share_of(data, me.champion).unwrap_or(0.5);
    let mix = magic_mix(data, me.champion, s);
    let lvl = me.level;
    let my_hp = base.hp_at(lvl) + s[STAT_HP];
    let my_armor = base.armor_at(lvl) + s[STAT_ARMOR];
    let my_mr = base.mr_at(lvl) + s[STAT_MR];
    let t = ITEM_STATS;
    // Synergies that need only the player's own build.
    let attack_speed = base.attack_speed * (1.0 + base.bonus_as_at(lvl) + s[STAT_AS]);
    v[t + 5] = s[STAT_CRIT].min(1.0) * (1.75 + s[STAT_CRIT_DMG]) * attack_speed / 2.0;
    v[t + 6] = b.on_hit * (base.bonus_as_at(lvl) + s[STAT_AS]);
    v[t + 7] = s[STAT_HASTE] / UNIT[STAT_HASTE] * caster;
    v[t + 10] = (s[STAT_CRIT] - 1.0).max(0.0) / UNIT[STAT_CRIT];
    v[t + 14] = (s[STAT_AP] / UNIT[STAT_AP] * (1.0 - own_magic) + s[STAT_AD] / UNIT[STAT_AD] * own_magic) / per_k;
    // (Not the build's gold or raw health: both grow with game time and acted as a clock, leaving the first model
    // overconfident late on synthetic data; the game state's gold difference already carries the lead.)
    v[t + 15] = actives(data, me.items) / 2.0;
    if !enemies.is_empty() {
        let n = enemies.len() as f64;
        let (mut armor, mut mr, mut hp, mut magic, mut heal) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let mut wounds = false;
        for e in enemies {
            let eb = build(data, e.items);
            let base_e = data.champion(e.champion).map(|c| c.base).unwrap_or_default();
            armor += base_e.armor_at(e.level) + eb.stats[STAT_ARMOR];
            mr += base_e.mr_at(e.level) + eb.stats[STAT_MR];
            hp += base_e.hp_at(e.level) + eb.stats[STAT_HP];
            magic += magic_mix(data, e.champion, &eb.stats);
            heal += eb.healing;
            wounds |= eb.wounds;
        }
        let (armor, mr, hp, magic) = (armor / n, mr / n, hp / n, magic / n);
        // Damage taken = 100 / (100 + effective resistance): % penetration first, then flat (lethality, flat magic pen).
        let eff_armor = (armor * (1.0 - s[STAT_ARMOR_PEN]) - s[STAT_LETHALITY]).max(0.0);
        let eff_mr = (mr * (1.0 - s[STAT_MAGIC_PEN_PCT]) - s[STAT_MAGIC_PEN]).max(0.0);
        let phys = 100.0 / (100.0 + eff_armor);
        let mag = 100.0 / (100.0 + eff_mr);
        v[t] = phys;
        v[t + 1] = mag;
        v[t + 2] = (1.0 - mix) * phys + mix * mag;
        let my_ehp = my_hp * (1.0 + ((1.0 - magic) * my_armor + magic * my_mr) / 100.0);
        let their_ehp = hp * (1.0 + ((1.0 - mix) * armor + mix * mr) / 100.0);
        v[t + 3] = ((1.0 - magic) * my_armor + magic * my_mr) / 100.0;
        v[t + 4] = (my_ehp.max(1.0) / their_ehp.max(1.0)).ln();
        v[t + 8] = (s[STAT_LIFE_STEAL] + s[STAT_OMNIVAMP]) / UNIT[STAT_LIFE_STEAL] * if wounds { 1.0 } else { 0.0 };
        v[t + 9] = if b.wounds { heal / 3.0 } else { 0.0 };
        v[t + 11] = s[STAT_MR] / UNIT[STAT_MR] * (1.0 - magic);
        v[t + 12] = s[STAT_ARMOR] / UNIT[STAT_ARMOR] * magic;
        v[t + 13] = (s[STAT_ARMOR_PEN] / UNIT[STAT_ARMOR_PEN] + s[STAT_LETHALITY] / UNIT[STAT_LETHALITY]) * ((80.0 - armor) / 80.0).max(0.0);
    } else {
        v[t + 3] = (my_armor + my_mr) / 200.0;
    }
    v.into_iter().map(|x| if x.is_finite() { x as f32 } else { 0.0 }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The damage formula on Data Dragon's numbers: Lord Dominik's lets more physical damage through, and more against
    /// armor-stacked enemies than against squishy ones.
    #[test]
    fn penetration_matters_more_against_armor() {
        let data = bf_core::testkit::load_test_static_data();
        let tank = [3075, 3742, 3047, 6333];
        let enemies_tanky: Vec<PlayerIn> = [54, 57, 516].iter().map(|c| PlayerIn { champion: *c, level: 16.0, items: &tank }).collect();
        let enemies_soft: Vec<PlayerIn> = [99, 103, 222].iter().map(|c| PlayerIn { champion: *c, level: 16.0, items: &[] }).collect();
        let without = [3031, 3094, 3006];
        let with = [3031, 3094, 3006, 3036];
        let through = |items: &[i64], enemies: &[PlayerIn]| combat_vector(&data, &PlayerIn { champion: 222, level: 16.0, items }, enemies)[ITEM_STATS] as f64;
        let gain_tanky = through(&with, &enemies_tanky) / through(&without, &enemies_tanky);
        let gain_soft = through(&with, &enemies_soft) / through(&without, &enemies_soft);
        assert!(gain_tanky > gain_soft && gain_soft > 1.0, "against tanks ×{gain_tanky:.3}, against squishies ×{gain_soft:.3}");
        // Stats per 1,000 gold: Infinity Edge alone is mostly attack damage and crit.
        let v = combat_vector(&data, &PlayerIn { champion: 222, level: 10.0, items: &[3031] }, &[]);
        assert!(v[STAT_AD] > 0.0 && v[STAT_CRIT] > 0.0 && v[STAT_AP] == 0.0);
        assert_eq!(v.len(), COMBAT);
    }
}
