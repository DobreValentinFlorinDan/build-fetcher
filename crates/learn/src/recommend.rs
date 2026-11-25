//! Recommendations (`impl BuildSource`).
//!
//! Every option shown carries its plain counts (games, wins), its recency- and rank-weighted games, and a shrunk
//! win rate and win probability added (WPA). Counts come from the store's tier B tables at four levels, most
//! specific first: the matchup (this lane opponent), the enemy damage profile, the lane (champion-role), the champion
//! (all roles). Each level's WPA is shrunk toward the next one's, so an option with few matchup games reads like its
//! lane numbers; the option's `basis` names the most specific level with enough games, and `thin` flags an option
//! that had to fall back to the champion's games.
//!
//! Tier A (a model trained on enough of this champion-role's games): the decisions made before the game (runes,
//! summoners, skill max, starting items) are ranked by the model's win chance with that option at the start of an
//! even game, averaged over recent situations of this champion-role with the user's draft put in (weighted by how
//! much their drafts look like the user's), minus the average with the choices actually made in them. The item path
//! is ranked by counts (see `MODEL_KINDS`); in game, `live` ranks next buys by the model given the game state.
//! Tier B: every section is ranked by the counts' shrunk WPA (wins − expected wins).

use crate::backend::Backend;
use crate::features::{ItemClasses, Situation, WeightOptions};
use crate::learner::{Learner, Served};
use anyhow::Result;
use bf_core::build::*;
use bf_core::ddragon::StaticData;
use bf_core::engine::exclusivity::{build_exclusivity_index, is_purchasable_item, ExclusivityIndex};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Draft-only contexts are judged at this minute (items owned by then count).
const EVEN_MINUTE: f64 = 20.0;

/// The sections tier A ranks by the model: decisions made before the game. The item path (first item, core, boots,
/// late items) is ranked by counts in both tiers: the model learns what an item adds given the game state, which is
/// what live advice needs (and uses), while at draft time an item's worth is mostly the lead it will build, which the
/// counts' wins minus expected wins measure directly.
const MODEL_KINDS: [OptionKind; 5] = [OptionKind::RunePage, OptionKind::Keystone, OptionKind::Summoners, OptionKind::SkillMax, OptionKind::StartingItems];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecommendConfig {
    pub weights: WeightOptions,
    /// Pseudo-games each level is shrunk with toward the next (the role's own level toward no effect). Why 250: one
    /// game's result has variance 0.25, and real option effects are a few points (sd ≈ 3 pts, variance ≈ 0.001), so
    /// the evidence of n games weighs n / (n + 0.25 / 0.001) ≈ n / (n + 250). 25 lucky games then move an estimate
    /// by a tenth of what they show, 1,000 games by four fifths.
    pub shrink: f64,
    /// A role with fewer games of this champion than this leans on the champion's other roles; above it, a role
    /// stands on its own (Summon Aery on support Ahri says nothing about mid Ahri).
    pub role_min_games: u32,
    /// Weighted games a level needs to be an option's basis.
    pub min_games: f64,
    /// Options shown per section.
    pub per_section: usize,
    /// Training examples a champion-role needs in the model for tier A.
    pub tier_a_min_samples: u64,
    /// Options per section the model scores (the most played ones).
    pub candidates: usize,
    /// Situations the model's win chances are averaged over.
    pub contexts: usize,
    /// Kernel width of the draft similarity.
    pub bandwidth: f64,
}

impl Default for RecommendConfig {
    fn default() -> Self {
        RecommendConfig {
            weights: WeightOptions::default(),
            shrink: 250.0,
            role_min_games: 200,
            min_games: 30.0,
            per_section: 5,
            tier_a_min_samples: 1500,
            candidates: 8,
            contexts: 64,
            bandwidth: 0.35,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Level {
    games: u32,
    wins: u32,
    wg: f64,
    ww: f64,
    we: f64,
}

impl Level {
    fn add(&mut self, c: &OptionCounts, w: f64) {
        self.games += c.games;
        self.wins += c.wins;
        self.wg += w * c.games as f64;
        self.ww += w * c.wins as f64;
        self.we += w * c.expected_wins;
    }
}

#[derive(Clone, Debug)]
struct Stats {
    option: BuildOption,
    matchup: Level,
    profile: Level,
    lane: Level,
    champion: Level,
}

/// An option's estimate from its counts.
#[derive(Clone, Debug)]
struct Estimate {
    wpa: f64,
    basis: Basis,
    level: Level,
    thin: bool,
}

/// Counts of one champion-role, aggregated per option and level.
struct Evidence {
    by_kind: BTreeMap<OptionKind, Vec<Stats>>,
    /// Each kind's weighted base win rate (Σ expected / Σ games at lane level).
    base: BTreeMap<OptionKind, f64>,
    lane_games: u32,
}

/// Display names from Data Dragon.
struct Names {
    runes: HashMap<i64, String>,
    summoners: HashMap<i64, String>,
}

pub struct Recommender {
    source: Arc<dyn DataSource>,
    learner: Option<Arc<Learner>>,
    data: Arc<StaticData>,
    classes: ItemClasses,
    excl: ExclusivityIndex,
    names: Names,
    backend: Arc<dyn Backend>,
    config: RecommendConfig,
}

/// Order within a group of equally supported options (call after splitting thin from supported): supported ones by
/// their estimate, then games; thin ones by games played.
fn by_evidence(a: &(f64, RankedOption), b: &(f64, RankedOption)) -> std::cmp::Ordering {
    if a.1.thin {
        b.1.weighted_games.total_cmp(&a.1.weighted_games)
    } else {
        b.0.total_cmp(&a.0).then(b.1.weighted_games.total_cmp(&a.1.weighted_games))
    }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Top => "top",
        Role::Jungle => "jungle",
        Role::Middle => "middle",
        Role::Bottom => "bottom",
        Role::Utility => "support",
    }
}

fn basis_name(b: Basis) -> &'static str {
    match b {
        Basis::Matchup => "matchup",
        Basis::Profile => "enemy profile",
        Basis::Lane => "lane",
        Basis::Champion => "champion",
    }
}

/// The parts of a rune page key: (keystone, sub style).
fn page_parts(key: &str) -> Option<(i64, i64)> {
    let mut it = key.split('/');
    let primary = it.next()?;
    let keystone = primary.split(':').nth(1)?.split(',').next()?.parse().ok()?;
    let sub = it.next()?.split(':').next()?.parse().ok()?;
    Some((keystone, sub))
}

impl Recommender {
    /// `learner`: None for counts only (tier B everywhere).
    pub fn new(source: Arc<dyn DataSource>, learner: Option<Arc<Learner>>, data: Arc<StaticData>, config: RecommendConfig) -> Recommender {
        let mut runes = HashMap::new();
        for t in &data.runes {
            runes.insert(t.id, t.name.clone());
            for s in &t.slots {
                for r in &s.runes {
                    runes.insert(r.id, r.name.clone());
                }
            }
        }
        let summoners = data.summoners.iter().map(|(id, s)| (*id, s.name.clone())).collect();
        let backend = learner.as_ref().map(|l| l.backend()).unwrap_or_else(|| Arc::new(crate::backend::CpuBackend));
        Recommender {
            source,
            learner,
            classes: ItemClasses::new(&data),
            excl: build_exclusivity_index(&data),
            names: Names { runes, summoners },
            data,
            backend,
            config,
        }
    }

    fn champ_name(&self, id: i64) -> String {
        self.data.champion(id).map(|c| c.name.clone()).unwrap_or_else(|| format!("#{id}"))
    }

    fn item_name(&self, id: i64) -> String {
        self.data.item(id).map(|i| i.name.clone()).unwrap_or_else(|| format!("#{id}"))
    }

    /// Display text for an option.
    pub fn label(&self, o: &BuildOption) -> String {
        let rune = |id: i64| self.names.runes.get(&id).cloned().unwrap_or_else(|| format!("#{id}"));
        match o.kind {
            OptionKind::RunePage => {
                let style: i64 = o.key.split(':').next().and_then(|s| s.parse().ok()).unwrap_or(0);
                match page_parts(&o.key) {
                    Some((k, sub)) => format!("{} · {} + {}", rune(k), rune(style), rune(sub)),
                    None => o.key.clone(),
                }
            }
            OptionKind::Keystone => o.key.parse().map(rune).unwrap_or_else(|_| o.key.clone()),
            OptionKind::Summoners => o
                .key
                .split(',')
                .filter_map(|x| x.parse::<i64>().ok())
                .map(|id| self.names.summoners.get(&id).cloned().unwrap_or_else(|| format!("#{id}")))
                .collect::<Vec<_>>()
                .join(" + "),
            OptionKind::SkillMax => o.key.replace('>', " > "),
            OptionKind::SkillOrder => o.key.replace(',', ", "),
            OptionKind::StartingItems => {
                let ids = o.item_ids();
                let mut parts: Vec<(i64, usize)> = vec![];
                for id in ids {
                    match parts.iter_mut().find(|p| p.0 == id) {
                        Some(p) => p.1 += 1,
                        None => parts.push((id, 1)),
                    }
                }
                parts.iter().map(|(id, n)| if *n > 1 { format!("{n}× {}", self.item_name(*id)) } else { self.item_name(*id) }).collect::<Vec<_>>().join(" + ")
            }
            OptionKind::CoreItems => o.item_ids().iter().map(|i| self.item_name(*i)).collect::<Vec<_>>().join(" > "),
            _ => o.item_ids().first().map(|i| self.item_name(*i)).unwrap_or_else(|| o.key.clone()),
        }
    }

    /// The role to use when the draft doesn't say: the champion's most played one, else one from its class.
    fn guess_role(&self, champion: i64) -> Role {
        let mut best: Option<(u32, Role)> = None;
        if let Ok(v) = self.source.volumes(&[]) {
            let mut per: BTreeMap<Role, u32> = BTreeMap::new();
            for x in v.iter().filter(|x| x.champion == champion) {
                *per.entry(x.role).or_default() += x.games;
            }
            for (r, g) in per {
                if best.map(|b| g > b.0).unwrap_or(true) {
                    best = Some((g, r));
                }
            }
        }
        if let Some((_, r)) = best {
            return r;
        }
        let tags = self.data.champion(champion).map(|c| c.tags.clone()).unwrap_or_default();
        match tags.first().map(|s| s.as_str()) {
            Some("Marksman") => Role::Bottom,
            Some("Support") => Role::Utility,
            Some("Mage") | Some("Assassin") => Role::Middle,
            _ => Role::Top,
        }
    }

    fn evidence(&self, champion: i64, role: Role, opponent: Option<i64>, profile: Option<DamageProfile>) -> Result<Evidence> {
        let mut per_role: Vec<(Role, Vec<OptionCounts>)> = vec![];
        for r in Role::ALL {
            per_role.push((r, self.source.counts(&CountQuery { champion, role: r, patches: vec![], kinds: vec![] })?));
        }
        // Age in patches among those with data (and the store's newest), newest = 0.
        let mut patches: Vec<Patch> = per_role.iter().flat_map(|(_, v)| v.iter().map(|c| c.patch)).collect();
        if let Some(p) = self.source.current_patch()? {
            patches.push(p);
        }
        patches.sort();
        patches.dedup();
        let o = &self.config.weights;
        let weight = |c: &OptionCounts| {
            let age = patches.iter().filter(|p| **p > c.patch).count();
            let rank = match c.rank {
                RankBand::MasterPlus => o.master_plus,
                RankBand::Diamond => o.diamond,
                RankBand::Mine => o.me,
            };
            bf_core::jsmath::pow(o.patch_decay, age as f64) * rank
        };
        let mut stats: BTreeMap<BuildOption, Stats> = BTreeMap::new();
        let mut lane_games = 0u32;
        for (r, counts) in &per_role {
            for c in counts {
                let w = weight(c);
                let s = stats.entry(c.option.clone()).or_insert_with(|| Stats { option: c.option.clone(), matchup: Level::default(), profile: Level::default(), lane: Level::default(), champion: Level::default() });
                let any = c.context == CountContext::default();
                if any {
                    s.champion.add(c, w);
                }
                if *r != role {
                    continue;
                }
                if any {
                    s.lane.add(c, w);
                    if c.option.kind == OptionKind::Summoners {
                        lane_games += c.games;
                    }
                } else if c.context.lane_opponent.is_some() && c.context.lane_opponent == opponent && c.context.enemy_profile.is_none() {
                    s.matchup.add(c, w);
                } else if c.context.enemy_profile.is_some() && c.context.enemy_profile == profile && c.context.lane_opponent.is_none() {
                    s.profile.add(c, w);
                }
            }
        }
        let mut by_kind: BTreeMap<OptionKind, Vec<Stats>> = BTreeMap::new();
        for s in stats.into_values() {
            by_kind.entry(s.option.kind).or_default().push(s);
        }
        let base = by_kind
            .iter()
            .map(|(k, v)| {
                let (we, wg) = v.iter().fold((0.0, 0.0), |a, s| if s.lane.wg > 0.0 { (a.0 + s.lane.we, a.1 + s.lane.wg) } else { (a.0 + s.champion.we, a.1 + s.champion.wg) });
                (*k, if wg > 0.0 { we / wg } else { 0.5 })
            })
            .collect();
        Ok(Evidence { by_kind, base, lane_games })
    }

    /// The option's win probability added in this role and situation, and the evidence it rests on.
    ///
    /// Each level is shrunk toward the next: the matchup and the enemy profile toward the role, the role toward no
    /// effect. The champion's other roles are the role's prior only when the role itself is thin (`role_thin`). The
    /// basis is the most specific level with `min_games`; an option without them in this role is `thin` and ranks
    /// after the supported ones (see `recommend_inner`).
    fn estimate(&self, s: &Stats, has_opp: bool, has_profile: bool, role_thin: bool) -> Estimate {
        let k = self.config.shrink;
        let shrink = |l: &Level, prior: f64| (l.ww - l.we + k * prior) / (l.wg + k);
        let role_prior = if role_thin { shrink(&s.champion, 0.0) } else { 0.0 };
        let lane = shrink(&s.lane, role_prior);
        let prof = if has_profile { shrink(&s.profile, lane) } else { lane };
        let wpa = if has_opp { shrink(&s.matchup, prof) } else { prof };
        let min = self.config.min_games;
        let (basis, level) = if has_opp && s.matchup.wg >= min {
            (Basis::Matchup, s.matchup)
        } else if has_profile && s.profile.wg >= min {
            (Basis::Profile, s.profile)
        } else if s.lane.wg >= min || !role_thin {
            // Shown with this role's own games, however few: never counts from other roles.
            (Basis::Lane, s.lane)
        } else {
            (Basis::Champion, s.champion)
        };
        Estimate { wpa, basis, level, thin: level.wg < min || basis == Basis::Champion }
    }

    fn ranked(&self, s: &Stats, e: &Estimate, base: f64, predicted: Option<f64>, wpa: f64) -> RankedOption {
        RankedOption {
            option: s.option.clone(),
            label: self.label(&s.option),
            games: e.level.games,
            wins: e.level.wins,
            weighted_games: e.level.wg,
            win_rate: (base + e.wpa).clamp(0.0, 1.0),
            wpa,
            predicted_win: predicted,
            basis: e.basis,
            thin: e.thin,
        }
    }

    /// Options a section may show at all: real items of the right class.
    fn eligible(&self, o: &BuildOption) -> bool {
        let ids = o.item_ids();
        match o.kind {
            OptionKind::Boots => ids.iter().all(|i| self.classes.boots.contains(i)),
            OptionKind::FirstItem | OptionKind::LateItem => ids.iter().all(|i| self.classes.completed.contains(i)),
            OptionKind::CoreItems => ids.iter().all(|i| self.classes.completed.contains(i)) && self.excl.conflicts(&ids).is_empty(),
            _ => true,
        }
    }

    fn situation_from_draft(&self, ctx: &DraftContext, role: Role, opponent: Option<i64>, profile: Option<DamageProfile>) -> Situation {
        Situation {
            champion: ctx.champion,
            role: Some(role),
            opponent,
            allies: ctx.allies.iter().map(|a| a.0).collect(),
            enemies: ctx.enemies.iter().map(|e| e.0).collect(),
            profile,
            state: crate::features::GameState { minute: 20.0, ..Default::default() },
            ..Default::default()
        }
    }

    /// The model's situations for this draft: recent ones of the champion-role with the user's draft put in, weighted
    /// by draft similarity (on the backend: the GPU when there is one).
    fn contexts(&self, served: &Served, ctx: &DraftContext, role: Role, opponent: Option<i64>, profile: Option<DamageProfile>) -> Vec<(Situation, f64)> {
        let own = served.contexts(ctx.champion, role);
        if own.is_empty() {
            return vec![(self.situation_from_draft(ctx, role, opponent, profile), 1.0)];
        }
        let share = |cs: &[i64]| DamageProfile::magic_share(&self.data, cs).unwrap_or(f64::NAN);
        let info = |c: Option<i64>| -> [f64; 3] {
            match c.and_then(|c| self.data.champion(c)) {
                Some(m) => [m.info.attack / 10.0, m.info.defense / 10.0, m.info.magic / 10.0],
                None => [f64::NAN; 3],
            }
        };
        const D: usize = 6;
        let mut flat = Vec::with_capacity(own.len() * D);
        for s in own {
            let o = info(s.opponent);
            flat.extend_from_slice(&[share(&s.enemies), share(&s.allies), o[0], o[1], o[2], if opponent.is_some() && s.opponent == opponent { 1.0 } else { 0.0 }]);
        }
        let allies: Vec<i64> = ctx.allies.iter().map(|a| a.0).collect();
        let enemies: Vec<i64> = ctx.enemies.iter().map(|a| a.0).collect();
        let o = info(opponent);
        let q = [share(&enemies), share(&allies), o[0], o[1], o[2], if opponent.is_some() { 1.0 } else { f64::NAN }];
        let mut w = vec![0.0; own.len()];
        let h = self.config.bandwidth;
        self.backend.similarities(&flat, own.len(), D, &q, 2.0 * h * h, &mut w);
        let mut idx: Vec<usize> = (0..own.len()).collect();
        idx.sort_by(|a, b| w[*b].total_cmp(&w[*a]).then(a.cmp(b)));
        idx.truncate(self.config.contexts);
        idx.into_iter()
            .map(|i| {
                let mut s = own[i].clone();
                if opponent.is_some() {
                    s.opponent = opponent;
                }
                if !enemies.is_empty() {
                    s.enemies = enemies.clone();
                    s.profile = profile.or(s.profile);
                }
                if !allies.is_empty() {
                    s.allies = allies.clone();
                }
                // An even game: a draft decision's worth is judged before anyone is ahead. With the real leads left in,
                // the model would only credit what an option adds beyond the lead it already helped build.
                s.state = crate::features::GameState { minute: s.state.minute.max(EVEN_MINUTE), ..Default::default() };
                (s, w[i].max(1e-12))
            })
            .collect()
    }

    /// `s` with option `o` chosen instead.
    fn apply(&self, s: &mut Situation, o: &BuildOption, pages: &HashMap<i64, String>) {
        let ids = o.item_ids();
        match o.kind {
            OptionKind::RunePage => {
                if let Some((k, sub)) = page_parts(&o.key) {
                    s.keystone = k;
                    s.sub_style = sub;
                }
                s.rune_page = Some(o.key.clone());
            }
            OptionKind::Keystone => {
                s.keystone = o.key.parse().unwrap_or(0);
                s.rune_page = pages.get(&s.keystone).cloned();
                if let Some((_, sub)) = s.rune_page.as_deref().and_then(page_parts) {
                    s.sub_style = sub;
                }
            }
            OptionKind::Summoners => s.summoners = Some(o.key.clone()),
            OptionKind::SkillMax => s.skill_max = Some(o.key.clone()),
            OptionKind::StartingItems => s.starting = Some(o.key.clone()),
            OptionKind::Boots => s.boots = ids.first().copied(),
            OptionKind::FirstItem => {
                s.items.retain(|x| !ids.contains(x));
                if s.items.is_empty() {
                    s.items.push(ids[0]);
                } else {
                    s.items[0] = ids[0];
                }
            }
            OptionKind::CoreItems => {
                let rest: Vec<i64> = s.items.iter().skip(3).copied().filter(|x| !ids.contains(x)).collect();
                s.items = ids.clone();
                s.items.extend(rest);
            }
            OptionKind::LateItem => {
                s.items.retain(|x| !ids.contains(x));
                if s.items.len() >= 4 {
                    s.items[3] = ids[0];
                } else {
                    s.items.push(ids[0]);
                }
            }
            OptionKind::SkillOrder => {}
        }
    }

    /// The model's win chance with each option (averaged over the contexts), and with the choices actually made.
    fn model_scores(&self, served: &Served, contexts: &[(Situation, f64)], options: &[BuildOption], pages: &HashMap<i64, String>) -> (Vec<f64>, f64) {
        // Pre-game decisions are judged at the start of an even game: their whole effect (see logistic.rs).
        let pregame: Vec<(Situation, f64)> = contexts
            .iter()
            .map(|(s, w)| {
                let mut x = s.clone();
                x.items.clear();
                x.boots = None;
                x.state = crate::features::GameState::default();
                (x, *w)
            })
            .collect();
        let contexts = &pregame;
        let wsum: f64 = contexts.iter().map(|c| c.1).sum();
        let avg = |f: &dyn Fn(&Situation) -> f64| contexts.iter().map(|(s, w)| w * f(s)).sum::<f64>() / wsum;
        let base = avg(&|s| served.predict(s));
        let scores = options
            .iter()
            .map(|o| {
                avg(&|s| {
                    let mut x = s.clone();
                    self.apply(&mut x, o, pages);
                    served.predict(&x)
                })
            })
            .collect();
        (scores, base)
    }

    fn served_for(&self, champion: i64, role: Role) -> Option<Arc<Served>> {
        let s = self.learner.as_ref()?.served()?;
        (s.pair_samples(champion, role) >= self.config.tier_a_min_samples).then_some(s)
    }

    /// Makes sections agree: the core build starts with the first item shown first where it can, late items repeat
    /// neither the core build nor its exclusivity groups, rune pages with the top keystone come first.
    fn reconcile(&self, sections: &mut [Section]) {
        let top = |sections: &[Section], k: OptionKind| sections.iter().find(|s| s.kind == k).and_then(|s| s.options.first().map(|o| o.option.clone()));
        let first = top(sections, OptionKind::FirstItem);
        let keystone = top(sections, OptionKind::Keystone);
        for s in sections.iter_mut() {
            match s.kind {
                OptionKind::CoreItems => {
                    if let Some(f) = first.as_ref().and_then(|f| f.item_ids().first().copied()) {
                        s.options.sort_by_key(|o| o.option.item_ids().first() != Some(&f));
                    }
                }
                OptionKind::RunePage => {
                    if let Some(k) = keystone.as_ref().and_then(|k| k.key.parse::<i64>().ok()) {
                        s.options.sort_by_key(|o| page_parts(&o.option.key).map(|p| p.0) != Some(k));
                    }
                }
                _ => {}
            }
        }
        let core = top(sections, OptionKind::CoreItems).map(|c| c.item_ids()).unwrap_or_default();
        for s in sections.iter_mut() {
            if s.kind == OptionKind::LateItem {
                s.options.retain(|o| o.option.item_ids().iter().all(|i| !core.contains(i) && self.excl.blocker(&core, *i).is_none()));
            }
            s.options.truncate(self.config.per_section);
        }
    }

    fn recommend_inner(&self, ctx: &DraftContext) -> Result<BuildRecommendation> {
        let mut notes = vec![];
        let role = match ctx.role {
            Some(r) => r,
            None => {
                let r = self.guess_role(ctx.champion);
                notes.push(format!("Role not assigned: showing {} as {}", self.champ_name(ctx.champion), role_name(r)));
                r
            }
        };
        let opponent = ctx.enemies.iter().find(|e| e.1 == Some(role)).map(|e| e.0);
        let enemies: Vec<i64> = ctx.enemies.iter().map(|e| e.0).collect();
        let profile = DamageProfile::of_team(&self.data, &enemies);
        let ev = self.evidence(ctx.champion, role, opponent, profile)?;
        let served = self.served_for(ctx.champion, role);
        let tier = if served.is_some() { Tier::A } else { Tier::B };
        let contexts = served.as_ref().map(|s| self.contexts(s, ctx, role, opponent, profile)).unwrap_or_default();
        // The most played page for each keystone (what a keystone swap means to the model).
        let mut pages: HashMap<i64, String> = HashMap::new();
        if let Some(v) = ev.by_kind.get(&OptionKind::RunePage) {
            let mut best: HashMap<i64, f64> = HashMap::new();
            for s in v {
                if let Some((k, _)) = page_parts(&s.option.key) {
                    if s.lane.wg > best.get(&k).copied().unwrap_or(-1.0) {
                        best.insert(k, s.lane.wg);
                        pages.insert(k, s.option.key.clone());
                    }
                }
            }
        }
        let mut sections = vec![];
        let mut fell_back: BTreeMap<&'static str, usize> = BTreeMap::new();
        for kind in OptionKind::ALL {
            let all = ev.by_kind.get(&kind).cloned().unwrap_or_default();
            let mut stats: Vec<Stats> = all.into_iter().filter(|s| self.eligible(&s.option) && (s.lane.games > 0 || s.champion.games > 0)).collect();
            // Most played first: the candidate list, and the order ties keep.
            stats.sort_by(|a, b| (b.lane.wg + 1e-3 * b.champion.wg).total_cmp(&(a.lane.wg + 1e-3 * a.champion.wg)).then(a.option.cmp(&b.option)));
            let base = ev.base.get(&kind).copied().unwrap_or(0.5);
            let role_thin = ev.lane_games < self.config.role_min_games;
            let est: Vec<Estimate> = stats.iter().map(|s| self.estimate(s, opponent.is_some(), profile.is_some(), role_thin)).collect();
            let mut options: Vec<(f64, RankedOption)> = vec![];
            let model = served.as_ref().filter(|_| MODEL_KINDS.contains(&kind) && !stats.is_empty());
            if let Some(sv) = model {
                let n = stats.len().min(self.config.candidates);
                let opts: Vec<BuildOption> = stats[..n].iter().map(|s| s.option.clone()).collect();
                let (scores, base_p) = self.model_scores(sv, &contexts, &opts, &pages);
                for i in 0..n {
                    let wpa = scores[i] - base_p;
                    options.push((wpa, self.ranked(&stats[i], &est[i], base, Some(scores[i]), wpa)));
                }
            } else {
                for (s, e) in stats.iter().zip(&est) {
                    options.push((e.wpa, self.ranked(s, e, base, None, e.wpa)));
                }
            }
            // Options with enough games in this role first, by their estimate (ties: more games first); thin ones after
            // them, most played first. A thin option's estimate rests on next to nothing, so it never outranks one the
            // data support, and among thin ones how often high-elo players pick it says more than a few results.
            options.sort_by(|a, b| a.1.thin.cmp(&b.1.thin).then_with(|| by_evidence(a, b)));
            for (_, o) in options.iter().take(self.config.per_section) {
                if o.basis != Basis::Matchup && opponent.is_some() {
                    *fell_back.entry(basis_name(o.basis)).or_default() += 1;
                }
            }
            sections.push(Section { kind, options: options.into_iter().map(|x| x.1).collect() });
        }
        self.reconcile(&mut sections);
        let games = ev.lane_games;
        let pair = format!("{} {}", self.champ_name(ctx.champion), role_name(role));
        if games == 0 {
            notes.push(format!("No games stored yet for {pair}"));
        } else if (games as f64) < self.config.min_games * 4.0 {
            notes.push(format!("Thin data: {games} games of {pair}; options marked thin fall back to the champion's games"));
        }
        if let Some(opp) = opponent {
            if let Some((b, n)) = fell_back.iter().max_by_key(|x| *x.1) {
                notes.push(format!("Matchup against {} is thin for {n} options: they use {b} data", self.champ_name(opp)));
            }
        }
        if let Some(s) = &served {
            notes.push(format!("Ranked by model {} over {} situations like this draft; counts shown beside", s.version(), contexts.len()));
        }
        notes.push("Several options, best first: the choice is yours".into());
        Ok(BuildRecommendation {
            champion: ctx.champion,
            role,
            patch: ctx.patch.or(self.source.current_patch()?),
            tier,
            lane_opponent: opponent,
            enemy_profile: profile,
            games,
            sections,
            model: served.map(|s| s.version()),
            notes,
        })
    }

    // -------------------------------------------------------------------------------------------- live

    /// The enemy's magic share from champions and, once they have items, their items; healing and resist stacking.
    fn enemy_build(&self, ctx: &LiveContext) -> (f64, usize, f64, f64) {
        let champs: Vec<i64> = if ctx.draft.enemies.is_empty() { ctx.enemy_items.iter().map(|e| e.0).collect() } else { ctx.draft.enemies.iter().map(|e| e.0).collect() };
        let champ_share = DamageProfile::magic_share(&self.data, &champs).unwrap_or(0.5);
        let (mut ap, mut ad, mut heal, mut armor, mut mr) = (0.0, 0.0, 0usize, 0.0, 0.0);
        for (_, items) in &ctx.enemy_items {
            for id in items {
                let Some(it) = self.data.item(*id) else { continue };
                let has = |t: &str| it.tags.iter().any(|x| x == t);
                if has("SpellDamage") {
                    ap += 1.0;
                }
                if has("Damage") || has("CriticalStrike") || has("AttackSpeed") {
                    ad += 1.0;
                }
                if has("LifeSteal") || has("SpellVamp") {
                    heal += 1;
                }
                armor += it.armor;
                mr += it.magic_resist;
            }
        }
        let share = if ap + ad > 0.0 { 0.5 * champ_share + 0.5 * ap / (ap + ad) } else { champ_share };
        (share, heal, armor, mr)
    }

    fn live_inner(&self, ctx: &LiveContext) -> Result<LiveAdvice> {
        let d = &ctx.draft;
        let role = d.role.unwrap_or_else(|| self.guess_role(d.champion));
        let opponent = d.enemies.iter().find(|e| e.1 == Some(role)).map(|e| e.0);
        let (share, heal, enemy_armor, enemy_mr) = self.enemy_build(ctx);
        let profile = Some(DamageProfile::from_magic_share(share));
        let ev = self.evidence(d.champion, role, opponent, profile)?;
        let role_thin = ev.lane_games < self.config.role_min_games;
        let owned = &ctx.my_items;
        let has_boots = owned.iter().any(|i| self.classes.boots.contains(i));
        let open = |id: i64| -> bool {
            !owned.contains(&id) && is_purchasable_item(self.data.item(id)) && self.excl.blocker(owned, id).is_none() && !(has_boots && self.classes.boots.contains(&id))
        };
        // Per item, the best evidence among the item sections (late items first: in game, that is the question).
        let mut cands: BTreeMap<i64, (Stats, Estimate, OptionKind)> = BTreeMap::new();
        for kind in [OptionKind::LateItem, OptionKind::FirstItem, OptionKind::Boots] {
            for s in ev.by_kind.get(&kind).into_iter().flatten() {
                let id = s.option.item_ids()[0];
                if !self.eligible(&s.option) || !open(id) || cands.contains_key(&id) {
                    continue;
                }
                let e = self.estimate(s, opponent.is_some(), profile.is_some(), role_thin);
                cands.insert(id, (s.clone(), e, kind));
            }
        }
        for s in ev.by_kind.get(&OptionKind::CoreItems).into_iter().flatten() {
            for id in s.option.item_ids() {
                if open(id) && !cands.contains_key(&id) {
                    let st = Stats { option: BuildOption::late_item(id), ..s.clone() };
                    let e = self.estimate(&st, opponent.is_some(), profile.is_some(), role_thin);
                    cands.insert(id, (st, e, OptionKind::LateItem));
                }
            }
        }
        let served = self.served_for(d.champion, role);
        let mut notes = vec![];
        let now = served.as_ref().map(|sv| {
            let x = self.live_situation(ctx, role, opponent, profile);
            (sv.predict(&x), x)
        });
        let mut next: Vec<(f64, RankedOption)> = cands
            .values()
            .map(|(s, e, kind)| {
                let base = ev.base.get(kind).copied().unwrap_or(0.5);
                let (wpa, pred) = match (&served, &now) {
                    (Some(sv), Some((p0, x))) => {
                        let mut y = x.clone();
                        if self.classes.boots.contains(&s.option.item_ids()[0]) {
                            y.boots = Some(s.option.item_ids()[0]);
                        } else {
                            y.items.push(s.option.item_ids()[0]);
                        }
                        let p = sv.predict(&y);
                        (p - p0, Some(p))
                    }
                    _ => (e.wpa, None),
                };
                (wpa, self.ranked(s, e, base, pred, wpa))
            })
            .collect();
        // As in champ select: items the data support in this role first, thin ones after.
        next.sort_by(|a, b| a.1.thin.cmp(&b.1.thin).then_with(|| by_evidence(a, b)).then(a.1.option.cmp(&b.1.option)));
        let mut next_buys: Vec<RankedOption> = next.into_iter().map(|x| x.1).take(self.config.per_section).collect();
        let magic_me = DamageProfile::magic_share_of(&self.data, d.champion).unwrap_or(0.5) >= 0.5;
        if next_buys.len() < 2 {
            notes.push(format!("Few games of {} {}: some options come from items that suit the champion, not from games", self.champ_name(d.champion), role_name(role)));
            for id in self.fallback_items(magic_me, &|id| open(id) && !next_buys.iter().any(|o| o.option.item_ids()[0] == id)) {
                if next_buys.len() >= 3 {
                    break;
                }
                next_buys.push(self.plain(id, Basis::Champion));
            }
        }
        // Counters to what the enemy builds.
        let mut wants: Vec<(String, Box<dyn Fn(&bf_core::ddragon::ItemMeta) -> bool>)> = vec![];
        if share >= 0.6 {
            wants.push((format!("Enemy damage is mostly magic (about {:.0}%)", share * 100.0), Box::new(|i| i.magic_resist > 0.0)));
        } else if share <= 0.4 {
            wants.push((format!("Enemy damage is mostly physical (about {:.0}%)", (1.0 - share) * 100.0), Box::new(|i| i.armor > 0.0)));
        }
        if heal >= 2 {
            wants.push((format!("Enemies build healing ({heal} items)"), Box::new(|i| i.anti_heal)));
        }
        if enemy_armor >= 150.0 && !magic_me {
            wants.push((format!("Enemies stack armor ({enemy_armor:.0})"), Box::new(|i| i.tags.iter().any(|t| t == "ArmorPenetration"))));
        }
        if enemy_mr >= 150.0 && magic_me {
            wants.push((format!("Enemies stack magic resist ({enemy_mr:.0})"), Box::new(|i| i.tags.iter().any(|t| t == "MagicPenetration"))));
        }
        let mut counters = vec![];
        for (reason, want) in &wants {
            let mut pool: Vec<i64> = self.classes.completed.iter().copied().filter(|id| open(*id) && self.data.item(*id).map(|i| want(i)).unwrap_or(false)).collect();
            // Items this champion actually builds first, then ones that suit its damage type.
            let played = |id: &i64| cands.get(id).map(|c| c.0.lane.wg + c.0.champion.wg).unwrap_or(0.0);
            let suits = |id: &i64| self.suits(*id, magic_me);
            pool.sort_by(|a, b| played(b).total_cmp(&played(a)).then(suits(b).cmp(&suits(a))).then(a.cmp(b)));
            for id in pool.into_iter().take(2) {
                let o = match cands.get(&id) {
                    Some((s, e, k)) => self.ranked(s, e, ev.base.get(k).copied().unwrap_or(0.5), None, e.wpa),
                    None => self.plain(id, Basis::Champion),
                };
                counters.push((o, reason.clone()));
            }
        }
        if let Some((p, _)) = &now {
            notes.push(format!("Model {} win chance now {:.0}%", served.as_ref().map(|s| s.version()).unwrap_or_default(), p * 100.0));
        }
        notes.push("Several options, best first: the choice is yours".into());
        Ok(LiveAdvice { next_buys, counters, predicted_win: now.map(|n| n.0), notes })
    }

    fn live_situation(&self, ctx: &LiveContext, role: Role, opponent: Option<i64>, profile: Option<DamageProfile>) -> Situation {
        let mut s = self.situation_from_draft(&ctx.draft, role, opponent, profile);
        s.items = ctx.my_items.iter().copied().filter(|i| self.classes.completed.contains(i)).collect();
        s.boots = ctx.my_items.iter().copied().find(|i| self.classes.boots.contains(i));
        let diff = |a: u8, b: u8| a as f64 - b as f64;
        s.state = crate::features::GameState {
            minute: ctx.game_time_s / 60.0,
            gold_diff: 0.0,
            cs_diff: ctx.opponent_cs.map(|c| ctx.my_cs as f64 - c as f64).unwrap_or(0.0),
            level_diff: ctx.opponent_level.map(|l| ctx.my_level as f64 - l as f64).unwrap_or(0.0),
            kill_diff: ctx.team_kills as f64 - ctx.enemy_kills as f64,
            tower_diff: diff(ctx.towers.0, ctx.towers.1),
            dragon_diff: diff(ctx.dragons.0, ctx.dragons.1),
            baron_diff: diff(ctx.barons.0, ctx.barons.1),
        };
        s
    }

    fn suits(&self, id: i64, magic: bool) -> bool {
        self.data.item(id).map(|i| i.tags.iter().any(|t| t == if magic { "SpellDamage" } else { "Damage" })).unwrap_or(false)
    }

    /// Completed items that suit the champion's damage type, most expensive first (when there are no games at all).
    fn fallback_items(&self, magic: bool, open: &dyn Fn(i64) -> bool) -> Vec<i64> {
        let mut v: Vec<i64> = self.classes.completed.iter().copied().filter(|id| open(*id) && self.suits(*id, magic)).collect();
        v.sort_by(|a, b| {
            let g = |id: &i64| self.data.item(*id).map(|i| i.gold_total).unwrap_or(0.0);
            g(b).total_cmp(&g(a)).then(a.cmp(b))
        });
        v
    }

    fn plain(&self, id: i64, basis: Basis) -> RankedOption {
        let option = if self.classes.boots.contains(&id) { BuildOption::boots(id) } else { BuildOption::late_item(id) };
        RankedOption { label: self.label(&option), option, games: 0, wins: 0, weighted_games: 0.0, win_rate: 0.5, wpa: 0.0, predicted_win: None, basis, thin: true }
    }
}

impl BuildSource for Recommender {
    fn recommend(&self, ctx: &DraftContext) -> Result<BuildRecommendation> {
        self.recommend_inner(ctx)
    }
    fn live(&self, ctx: &LiveContext) -> Result<LiveAdvice> {
        self.live_inner(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_keys_parse() {
        let k = BuildOption::rune_page(8000, &[8010, 9111, 9104, 8299], 8400, &[8444, 8473], &[5008, 5008, 5011]).key;
        assert_eq!(page_parts(&k), Some((8010, 8400)));
    }
}
