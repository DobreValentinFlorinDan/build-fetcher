//! Port of `server/model/live.ts` (B5 live re-ranking). The stability rule here; the re-ranker with the Live Client
//! (`LiveReranker`) is in live_rerank.rs.

use super::estimate::OptionEstimate;

#[derive(Clone, Copy, Debug)]
pub struct StabilityOptions {
    pub delta: f64,
    pub polls: u32,
}
pub const STABILITY_DEFAULTS: StabilityOptions = StabilityOptions { delta: 0.01, polls: 3 };

/// The stability rule for "Next".
#[derive(Clone, Debug)]
pub struct NextStability {
    pub current: Option<String>,
    slot: i64,
    challenger: Option<(String, u32)>,
    o: StabilityOptions,
}

impl NextStability {
    pub fn new(o: StabilityOptions) -> NextStability {
        NextStability { current: None, slot: -1, challenger: None, o }
    }
    pub fn reset(&mut self) {
        self.current = None;
        self.slot = -1;
        self.challenger = None;
    }
    /// `options` best first. Returns true when "Next" changed.
    pub fn step(&mut self, slot: i64, options: &[OptionEstimate], in_progress: Option<&str>) -> bool {
        if options.is_empty() {
            let had = self.current.is_some();
            self.reset();
            return had;
        }
        let top = &options[0];
        let current = if slot == self.slot { options.iter().find(|x| Some(&*x.option) == self.current.as_deref()) } else { None };
        let Some(current) = current else {
            self.current = Some(match in_progress {
                Some(p) if options.iter().any(|x| &*x.option == p) => p.to_string(),
                _ => top.option.to_string(),
            });
            self.slot = slot;
            self.challenger = None;
            return true;
        };
        if top.option != current.option && top.theta - current.theta >= self.o.delta && in_progress != Some(&*current.option) {
            let polls = match &self.challenger {
                Some((o, p)) if **o == *top.option => p + 1,
                _ => 1,
            };
            self.challenger = Some((top.option.to_string(), polls));
            if polls >= self.o.polls {
                self.current = Some(top.option.to_string());
                self.challenger = None;
                return true;
            }
            return false;
        }
        self.challenger = None;
        false
    }
}

// ------------------------------------------------------------------------------------------- re-ranker

use super::context::{draft_context, item_dim, live_context, standardise, ChampRef, EnemyNow, LiveSituation};
use super::draft::{driving_dimension, observed, weigh_games, DraftModel, DraftSettings};
use super::estimate::{estimate_decision, EstimateOptions, ObsRef, Observation, Query, Z};
use super::weights::RegionPatchFn;
use crate::ddragon::StaticData;
use crate::engine::adaptive::identify;
use crate::engine::exclusivity::{is_purchasable_item, ExclusivityIndex};
use crate::engine::validator::is_completed_item;
use crate::types::{to_lane, PickStats};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const IRELIA: &str = "Irelia";

fn to_stats(e: &OptionEstimate) -> PickStats {
    PickStats { lift: e.theta, low: e.low, high: e.high, n_eff: e.n_eff, games: e.games, pilots: e.pilots, win_rate: e.win_rate, matchup_games: e.matchup_games, start_win: e.start_win }
}

/// Every component (recursively) an item is built from.
fn components_of(data: &StaticData, id: i64, out: &mut Vec<i64>) {
    if let Some(it) = data.item(id) {
        for c in &it.from {
            if !out.contains(c) {
                out.push(*c);
            }
            components_of(data, *c, out);
        }
    }
}

/// The live state the re-ranker reads, in the model's terms.
#[derive(Clone, Debug)]
pub struct LiveQuery {
    pub lane: &'static str,
    pub slot: usize,
    pub owned: Vec<i64>,
    pub opponent: Option<i64>,
    pub z: Vec<f64>,
    pub raw: Vec<f64>,
    pub fed_enemy: Option<(String, bool)>,
}

/// The app's matchup in game: who you face and your lane. `opponent_name`: None → your position decides.
#[derive(Clone, Debug, Default)]
pub struct LiveHint {
    pub opponent_name: Option<Option<String>>,
    pub lane: Option<&'static str>,
}

fn items_of(p: &Value) -> Vec<(i64, f64)> {
    p["items"].as_array().map(|a| a.iter().map(|i| (i["itemID"].as_i64().unwrap_or(0), i["count"].as_f64().unwrap_or(f64::NAN))).collect()).unwrap_or_default()
}

pub fn live_query(raw: &Value, model: &DraftModel, data: &StaticData, hint: &LiveHint) -> Option<LiveQuery> {
    let (me_i, opp_i) = identify(raw, hint.opponent_name.as_ref().map(|o| o.as_deref()));
    let players = raw["allPlayers"].as_array()?;
    let me = &players[me_i?];
    let name = |p: &Value| p["championName"].as_str().unwrap_or("").to_string();
    if data.champion_by_key(&name(me)).map(|c| c.key.as_str()) != Some(IRELIA) {
        return None;
    }
    let lane = hint.lane.or_else(|| to_lane(me["position"].as_str())).unwrap_or("TOP");
    let owned: Vec<i64> = items_of(me).into_iter().map(|x| x.0).filter(|id| *id > 0).collect();
    let slot = owned.iter().filter(|id| is_completed_item(data.item(**id))).count();
    if slot >= 6 {
        return None;
    }
    let champ = |p: &Value| data.champion_by_key(&name(p)).map(|c| c.id);
    let me_idx = me_i.unwrap();
    let enemies: Vec<&Value> = players.iter().filter(|p| p["team"] != me["team"]).collect();
    let allies: Vec<&Value> = players.iter().enumerate().filter(|(i, p)| p["team"] == me["team"] && *i != me_idx).map(|(_, p)| p).collect();
    let ids = |ps: &[&Value]| ps.iter().filter_map(|p| champ(p)).collect::<Vec<i64>>();
    let opponent = opp_i.map(|i| &players[i]);
    let opp_id = opponent.and_then(champ);
    let draft = draft_context(
        &model.profiles,
        &ids(&enemies).iter().map(|id| ChampRef { id: *id, position: None }).collect::<Vec<_>>(),
        &ids(&allies).iter().map(|id| ChampRef { id: *id, position: None }).collect::<Vec<_>>(),
        opp_id.map(|id| ChampRef { id, position: Some(lane) }),
    );
    let value = |p: &Value| -> f64 {
        let mut s = 0.0;
        for (id, count) in items_of(p) {
            let c = if count.is_nan() { 1.0 } else { count };
            s += data.item(id).map(|i| i.gold_total).unwrap_or(0.0) * super::logistic::js_max(1.0, c);
        }
        s
    };
    let team = |ps: &[&Value]| ps.iter().fold(0.0, |s, p| s + value(p));
    let kills = |ps: &[&Value]| ps.iter().fold(0.0, |s, p| s + p["scores"]["kills"].as_f64().unwrap_or(0.0));
    let mut mine: Vec<&Value> = vec![me];
    mine.extend(allies.iter().copied());
    let enemy_items: Vec<(Arc<super::profiles::ChampionProfile>, Vec<f64>)> = enemies
        .iter()
        .filter_map(|p| champ(p).map(|id| (model.profiles.get(id, None), items_of(p).into_iter().map(|x| x.0 as f64).collect())))
        .collect();
    let enemy_now: Vec<EnemyNow> = enemy_items.iter().map(|(profile, items)| EnemyNow { profile: profile.clone(), items }).collect();
    let live = live_context(
        &enemy_now,
        &|id| data.item(id as i64),
        &LiveSituation {
            lane_gold: opponent.map(|o| value(me) - value(o)).unwrap_or(f64::NAN),
            team_gold: team(&mine) - team(&enemies),
            kills: kills(&mine) - kills(&enemies),
            minute: raw["gameData"]["gameTime"].as_f64().unwrap_or(f64::NAN) / 60.0,
        },
    );
    let avg = enemies.iter().fold(0.0, |s, p| s + value(p)) / (enemies.len().max(1)) as f64;
    let mut sorted = enemies.clone();
    sorted.sort_by(|a, b| crate::engine::recommend::js_order(value(b) - value(a)));
    let fed = sorted.first().copied().filter(|top| avg > 0.0 && value(top) >= 1.25 * avg);
    let fed_profile = fed.and_then(|f| champ(f)).map(|id| model.profiles.get(id, None));
    let mut z = standardise(&draft, &model.draft_scale);
    z.extend(standardise(&live, &model.live_scale));
    let mut rawv = draft.clone();
    rawv.extend(live.iter());
    Some(LiveQuery {
        lane,
        slot,
        owned,
        opponent: opp_id,
        z,
        raw: rawv,
        fed_enemy: match (fed, fed_profile) {
            (Some(f), Some(p)) => Some((data.champion_by_key(&name(f)).map(|c| c.name.clone()).unwrap_or_else(|| name(f)), p.magic_share > 0.5)),
            _ => None,
        },
    })
}

/// How a reason chip names a live dimension; the threat-weighted damage mix names the most fed enemy.
fn reason_text(dim: usize, q: &LiveQuery) -> String {
    let d = item_dim(dim);
    if d.key == "eMagicThreat" {
        if let Some((name, ap)) = &q.fed_enemy {
            return format!("fed {} ({})", name, if *ap { "AP" } else { "AD" });
        }
    }
    format!("{} {}", d.label, (d.format)(q.raw[dim]))
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveAlternative {
    pub item_id: i64,
    pub stats: PickStats,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveNext {
    pub item_id: i64,
    pub slot: usize,
    pub reason: Option<String>,
    pub stats: PickStats,
    pub in_progress: bool,
    pub alternatives: Vec<LiveAlternative>,
}

/// Re-scores the next item slot on every poll and keeps "Next" stable.
pub struct LiveReranker {
    stable: NextStability,
    reason_text: Option<String>,
    obs_cache: Option<(String, HashMap<usize, ObsRef>)>,
    /// Milliseconds the last re-rank took.
    pub last_ms: f64,
}

impl Default for LiveReranker {
    fn default() -> Self {
        LiveReranker::new(STABILITY_DEFAULTS)
    }
}

fn est_options(s: &DraftSettings) -> EstimateOptions {
    match s.item_k {
        Some(k) => EstimateOptions { k, ..s.estimate },
        None => s.estimate,
    }
}

fn num(o: &str) -> f64 {
    crate::js::to_number(&Value::String(o.into()))
}

impl LiveReranker {
    pub fn new(stability: StabilityOptions) -> LiveReranker {
        LiveReranker { stable: NextStability::new(stability), reason_text: None, obs_cache: None, last_ms: 0.0 }
    }
    pub fn reset(&mut self) {
        self.stable.reset();
        self.reason_text = None;
    }
    /// Lets go of the observations kept for the game; whether there were any.
    pub fn release(&mut self) -> bool {
        self.obs_cache.take().is_some()
    }

    fn observations(&mut self, model: &DraftModel, key: String, data: &Arc<StaticData>, region_patch: &RegionPatchFn, s: &DraftSettings, slot: usize) -> ObsRef {
        if self.obs_cache.as_ref().map(|c| c.0 != key).unwrap_or(true) {
            self.obs_cache = Some((key, HashMap::new()));
        }
        let cache = &mut self.obs_cache.as_mut().unwrap().1;
        if let Some(o) = cache.get(&slot) {
            return o.clone();
        }
        let weighted = weigh_games(model, data, region_patch, &s.weights);
        let mut list = Vec::new();
        for wg in &weighted.list {
            let sg = &model.scored[wg.i];
            let Some(c) = sg.g.items.get(slot) else { continue };
            let w = wg.base * weighted.patch(&sg.g, &[c.item as i64]);
            if w > 0.0 {
                list.push(Observation {
                    option: super::draft::opt_str(c.item),
                    wpa: sg.item_wpa[slot],
                    win: sg.g.win,
                    start_win: sg.item_w0[slot],
                    w,
                    lane: sg.g.lane,
                    opponent: sg.g.opponent,
                    pilot: Arc::from(sg.g.pilot.as_str()),
                    z: Z::Row(model.z_items.clone(), model.item_row[wg.i] as usize + slot),
                    row: None,
                    age: Some(wg.age),
                });
            }
        }
        let obs = observed(list, &s.weights);
        cache.insert(slot, obs.clone());
        obs
    }

    /// `version` identifies the model build (the observation cache is kept while it holds).
    #[allow(clippy::too_many_arguments)]
    pub fn rerank(&mut self, model: &DraftModel, version: &str, raw: &Value, data: &Arc<StaticData>, excl: &ExclusivityIndex, region_patch: &RegionPatchFn, s: &DraftSettings, hint: &LiveHint) -> Option<LiveNext> {
        let t0 = std::time::Instant::now();
        let out = self.rerank_inner(model, version, raw, data, excl, region_patch, s, hint);
        self.last_ms = t0.elapsed().as_secs_f64() * 1000.0;
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn rerank_inner(&mut self, model: &DraftModel, version: &str, raw: &Value, data: &Arc<StaticData>, excl: &ExclusivityIndex, region_patch: &RegionPatchFn, s: &DraftSettings, hint: &LiveHint) -> Option<LiveNext> {
        let Some(q) = live_query(raw, model, data, hint) else {
            self.reset();
            return None;
        };
        let obs = self.observations(model, format!("{}|{:?}", version, s), data, region_patch, s, q.slot);
        let query = Query { lane: q.lane, opponent: q.opponent, z: q.z.clone(), sims: None, memo: None };
        let eo = est_options(s);
        let est = estimate_decision(&obs, &query, &eo, None, None);
        let options: Vec<OptionEstimate> = est
            .options
            .into_iter()
            .filter(|o| {
                let id = num(&o.option);
                id.fract() == 0.0 && is_purchasable_item(data.item(id as i64)) && excl.blocker(&q.owned, id as i64).is_none()
            })
            .collect();
        if options.is_empty() {
            self.reset();
            return None;
        }
        let owned: HashSet<i64> = q.owned.iter().copied().collect();
        let mut in_progress: Option<(i64, f64)> = None;
        for o in &options {
            let id = num(&o.option) as i64;
            let mut comps = Vec::new();
            components_of(data, id, &mut comps);
            let gold = comps.iter().filter(|c| owned.contains(c)).fold(0.0, |s, c| s + data.item(*c).map(|i| i.gold_total).unwrap_or(0.0));
            if gold > 0.0 && in_progress.map(|p| gold > p.1).unwrap_or(true) {
                in_progress = Some((id, gold));
            }
        }
        let ip = in_progress.map(|p| crate::js::num_to_string(p.0 as f64));
        let changed = self.stable.step(q.slot as i64, &options, ip.as_deref());
        if changed {
            self.reason_text = None;
        }
        let cur = num(self.stable.current.as_deref().unwrap_or(""));
        let chosen = options.iter().find(|o| num(&o.option) == cur)?.clone();
        if changed {
            self.reason_text = options.iter().find(|o| o.option != chosen.option).and_then(|rival| {
                driving_dimension(&chosen.option, &rival.option, &obs, &query, &eo, &|j| reason_text(j, &q))
            });
        }
        Some(LiveNext {
            item_id: num(&chosen.option) as i64,
            slot: q.slot + 1,
            reason: self.reason_text.clone(),
            stats: to_stats(&chosen),
            in_progress: in_progress.map(|p| p.0 as f64 == num(&chosen.option)).unwrap_or(false),
            alternatives: options.iter().filter(|o| o.option != chosen.option).take(3).map(|o| LiveAlternative { item_id: num(&o.option) as i64, stats: to_stats(o) }).collect(),
        })
    }
}
