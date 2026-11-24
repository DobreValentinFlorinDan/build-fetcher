//! Port of `server/model/dataset.ts`: the stored Irelia games as the draft model sees them (every decision, the draft
//! around it, and for items the game state when it was bought), and their win-probability-added.

use super::context::{draft_context, live_context, ChampRef, EnemyNow, LiveSituation};
use super::profiles::ChampionProfiles;
use super::state::{frame_states, state_features, states_at, train_state_model, win_probability_of, GameStateInput, Inventories, StateModel, TimelineGame, N_FEATURES};
use super::swaps::{swap_decisions, SWAP_DEFAULTS};
use super::window::{ends_of, frame_at_or_before, interval_frames, item_delta, purchase_lives, StateSample, WindowEnd, SAMPLE_SEED};
use crate::capture::TimelineCapture;
use crate::db::{IreliaGameRow, ParticipantLite};
use crate::ddragon::StaticData;
use crate::engine::recommend::derive_priority;
use crate::engine::validator::{is_tier2_boots, validate_game, Verdict};
use crate::types::to_lane;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

const IRELIA: i64 = 39;

/// One item decision: what, when, the game state just before (state-model features) and the live context.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemChoice {
    pub item: f64,
    pub t: f64,
    pub x: Option<Box<[f64; N_FEATURES]>>,
    pub live: [f64; 8],
}

/// A late-game swap decision.
#[derive(Clone, Debug, PartialEq)]
pub struct SwapChoice {
    pub item: f64,
    pub t: f64,
    pub x: Option<Box<[f64; N_FEATURES]>>,
    pub live: [f64; 8],
    pub held: f64,
    pub w: f64,
    /// The win chance at the visit under each fold's state model (evaluation).
    pub p: Option<Vec<f64>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedGame {
    pub match_id: String,
    pub platform: String,
    pub patch: String,
    pub created: f64,
    pub pilot: String,
    pub source: String,
    pub tier: Option<String>,
    pub lp: Option<f64>,
    pub mastery: Option<f64>,
    pub lane: &'static str,
    /// Grouped drafts (0.20.0): this game's draft group in a trial.
    pub dg: Option<usize>,
    /// The evaluation's index of this game (games, then older games): where its purchases' chances are.
    pub ix: usize,
    pub win: f64,
    pub opponent: Option<i64>,
    pub enemies: Vec<i64>,
    pub allies: Vec<i64>,
    pub draft: [f64; 12],
    pub primary: i64,
    pub sub: i64,
    pub perks: Vec<i64>,
    pub shards: Vec<i64>,
    pub summoners: String,
    pub start: String,
    pub max_first: Option<String>,
    pub early: Option<String>,
    pub items: Vec<ItemChoice>,
    pub boots: Option<ItemChoice>,
    pub swaps: Vec<SwapChoice>,
    /// State-model rows (f32, as 0.x keeps them), one per frame from 1:00.
    pub frames: Option<Vec<f32>>,
    /// When each of `frames` was (ms): what the 5-minute sample reads (2.2.0).
    pub frame_t: Option<Vec<f64>>,
    pub p_at: Option<Vec<f64>>,
    pub purchases: Vec<(i64, f64)>,
    /// When each of `items` left the inventory, sold or consumed into an upgrade (None: kept to the end); empty without
    /// a timeline (2.2.0's effective window).
    pub item_ends: Vec<Option<WindowEnd>>,
    pub boots_end: Option<WindowEnd>,
    /// The same for each of `purchases` (frame only, no features): what the best buy reads.
    pub purchase_ends: Vec<Option<WindowEnd>>,
    pub kda: [f64; 3],
    pub duration: f64,
    pub lane10: Option<f64>,
    pub lane15: Option<f64>,
}

/// A prepared game scored with a state model.
#[derive(Clone, Debug)]
pub struct ScoredGame {
    pub g: Arc<PreparedGame>,
    pub wpa0: f64,
    pub item_wpa: Vec<f64>,
    pub boots_wpa: Option<f64>,
    pub swap_wpa: Vec<f64>,
    /// The state model's win chance just before each core item and the boots (as `item_wpa`/`boots_wpa` use it).
    pub item_w0: Vec<f64>,
    pub boots_w0: Option<f64>,
}

/// Skill maxed first (needs nine skill-ups), and the first three.
pub fn skill_choices(order: &str) -> (Option<String>, Option<String>) {
    let path: Vec<String> = order.chars().map(|c| c.to_string()).collect();
    let max_first = if path.len() >= 9 { derive_priority(&path).into_iter().next() } else { None };
    let early = if path.len() >= 3 { Some(path[..3].concat()) } else { None };
    (max_first, early)
}

/// The live context of a purchase when the game has no timeline: only the minute is known.
pub fn live_unknown(t: f64) -> [f64; 8] {
    let n = f64::NAN;
    [n, n, n, n, n, n, n, t / 60_000.0]
}

/// A stored game's timeline seen from Irelia, and whose participant ids are whose.
pub struct GameTimeline {
    pub cap: TimelineCapture,
    pub irelia: f64,
    pub opponent: Option<f64>,
    pub teams: HashMap<i64, f64>,
    pub enemies: Vec<(f64, i64)>,
}

impl GameTimeline {
    pub fn team_of(&self, id: f64) -> f64 {
        self.teams.get(&(id as i64)).copied().unwrap_or(if id > 5.0 { 200.0 } else { 100.0 })
    }
}

pub fn game_timeline(row: &IreliaGameRow, ps: &[&ParticipantLite]) -> Option<GameTimeline> {
    let text = row.timeline.as_deref().filter(|t| !t.is_empty())?;
    let me = ps.iter().find(|p| Some(p.participant_id) == row.participant_id).or_else(|| ps.iter().find(|p| p.champion_id == IRELIA));
    let pid = me.map(|p| p.participant_id).or(row.participant_id)?;
    let cap = TimelineCapture::parse(text)?;
    if !cap.frames.contains_key(&(pid as u32)) {
        return None;
    }
    let my_team = me.map(|p| p.team_id).unwrap_or(if pid > 5 { 200 } else { 100 });
    let mut teams = HashMap::new();
    for p in ps {
        teams.entry(p.participant_id).or_insert(p.team_id as f64);
    }
    let enemy_ps: Vec<&&ParticipantLite> = ps.iter().filter(|p| p.team_id != my_team).collect();
    let opp = row.opponent_champion_id.filter(|o| *o != 0).and_then(|o| enemy_ps.iter().find(|p| p.champion_id == o)).map(|p| p.participant_id as f64);
    Some(GameTimeline {
        cap,
        irelia: pid as f64,
        opponent: opp,
        teams,
        enemies: enemy_ps.iter().map(|p| (p.participant_id as f64, p.champion_id)).collect(),
    })
}

pub struct Moment {
    pub x: [f64; N_FEATURES],
    pub live: [f64; 8],
    pub owned: Vec<f64>,
    pub state: GameStateInput,
}

/// The state-model features, the live context and Irelia's inventory at each of `times` (ms) of a stored game.
pub fn moments_at(t: &GameTimeline, times: &[f64], profiles: &ChampionProfiles, data: &StaticData) -> Vec<Moment> {
    let price_of = |id: f64| data.items.get(&(id as i64)).map(|i| i.gold_total).unwrap_or(0.0);
    let team_of = |id: f64| t.team_of(id);
    let tg = TimelineGame { cap: &t.cap, irelia: t.irelia, opponent: t.opponent, team_of: &team_of, price_of: &price_of };
    let mut enemy_items: Vec<Vec<Vec<f64>>> = vec![vec![]; times.len()];
    let mut owned: Vec<Vec<f64>> = vec![vec![]; times.len()];
    let states = {
        let mut cb = |i: usize, inv: &Inventories| {
            enemy_items[i] = t.enemies.iter().map(|(pid, _)| inv.get(&(*pid as i64)).cloned().unwrap_or_default()).collect();
            owned[i] = inv.get(&(t.irelia as i64)).cloned().unwrap_or_default();
        };
        states_at(&tg, times, Some(&mut cb))
    };
    let item_of = |id: f64| data.items.get(&(id as i64));
    times
        .iter()
        .enumerate()
        .map(|(i, at)| {
            let s = states[i];
            let enemies: Vec<EnemyNow> =
                t.enemies.iter().enumerate().map(|(j, (_, champ))| EnemyNow { profile: profiles.get(*champ, None), items: &enemy_items[i][j] }).collect();
            let live = live_context(
                &enemies,
                &item_of,
                &LiveSituation {
                    lane_gold: match (s.me, s.opponent) {
                        (Some(m), Some(o)) => m.inventory - o.inventory,
                        _ => f64::NAN,
                    },
                    team_gold: s.us.inventory - s.them.inventory,
                    kills: s.us.kills - s.them.kills,
                    minute: at / 60_000.0,
                },
            );
            let mut l = [0.0; 8];
            l.copy_from_slice(&live);
            Moment { x: state_features(&s), live: l, owned: std::mem::take(&mut owned[i]), state: s }
        })
        .collect()
}

/// `me`'s total gold and CS minus `opp`'s at `minute` (the first frame at or after it).
pub fn lane_diff(cap: &TimelineCapture, me: f64, opp: Option<f64>, minute: f64) -> Option<(f64, f64)> {
    let opp = opp?;
    let i = cap.t.iter().position(|t| *t >= minute * 60_000.0)?;
    let a = cap.frames.get(&(me as u32))?.get(i)?;
    let b = cap.frames.get(&(opp as u32))?.get(i)?;
    Some((a[0] - b[0], a[4] - b[4]))
}

pub struct PrepareOptions {
    pub starting_item_window_ms: f64,
}

/// Groups the participants by match (the rows prepare_game reads).
pub fn by_match(participants: &[ParticipantLite]) -> HashMap<&str, Vec<&ParticipantLite>> {
    let mut m: HashMap<&str, Vec<&ParticipantLite>> = HashMap::new();
    for p in participants {
        m.entry(p.match_id.as_str()).or_default().push(p);
    }
    m
}

/// One stored game prepared (None: outside top and mid, or not valid).
pub fn prepare_game(row: &Arc<IreliaGameRow>, ps: &[&ParticipantLite], data: &StaticData, profiles: &ChampionProfiles, o: &PrepareOptions) -> Option<PreparedGame> {
    let lane = to_lane(Some(&row.position))?;
    let game = match validate_game(row, data, &|_| Some(1.0), o.starting_item_window_ms) {
        Verdict::Ok(g) => g,
        Verdict::Bad { .. } => return None,
    };
    let me = ps.iter().find(|p| Some(p.participant_id) == row.participant_id).or_else(|| ps.iter().find(|p| p.champion_id == IRELIA));
    let pid = me.map(|p| p.participant_id).or(row.participant_id);
    let my_team = me.map(|p| p.team_id).unwrap_or(match pid {
        Some(p) if p > 5 => 200,
        _ => 100,
    });
    let enemy_ps: Vec<&&ParticipantLite> = ps.iter().filter(|p| p.team_id != my_team).collect();
    let ally_ps: Vec<&&ParticipantLite> =
        ps.iter().filter(|p| p.team_id == my_team && !me.map(|m| std::ptr::eq(*m, **p)).unwrap_or(false)).collect();
    let opponent = row.opponent_champion_id;
    let draft_v = draft_context(
        profiles,
        &enemy_ps.iter().map(|p| ChampRef { id: p.champion_id, position: None }).collect::<Vec<_>>(),
        &ally_ps.iter().map(|p| ChampRef { id: p.champion_id, position: None }).collect::<Vec<_>>(),
        opponent.filter(|o| *o != 0).map(|id| ChampRef { id, position: Some(&row.position) }),
    );
    let mut draft = [0.0; 12];
    draft.copy_from_slice(&draft_v);

    let mut first_buy: HashMap<i64, f64> = HashMap::new();
    for (id, ts) in &game.purchases {
        first_buy.entry(*id).or_insert(*ts);
    }
    let item_times: Vec<(f64, f64)> = game.completed.iter().take(6).map(|id| (*id as f64, first_buy[id])).collect();
    let boots_time = game.boots.map(|b| (b as f64, first_buy[&b]));

    let mut frames: Option<Vec<f32>> = None;
    let mut frame_t: Option<Vec<f64>> = None;
    let mut item_ends: Vec<Option<WindowEnd>> = Vec::new();
    let mut boots_end: Option<WindowEnd> = None;
    let mut purchase_ends: Vec<Option<WindowEnd>> = Vec::new();
    let mut lane10 = None;
    let mut lane15 = None;
    let mut choices: Vec<ItemChoice> = item_times.iter().map(|(item, t)| ItemChoice { item: *item, t: *t, x: None, live: live_unknown(*t) }).collect();
    let mut boots: Option<ItemChoice> = boots_time.map(|(item, t)| ItemChoice { item, t, x: None, live: live_unknown(t) });
    let mut swaps: Vec<SwapChoice> = Vec::new();
    if let Some(tl) = game_timeline(row, ps) {
        let price_of = |id: f64| data.items.get(&(id as i64)).map(|i| i.gold_total).unwrap_or(0.0);
        let team_of = |id: f64| tl.team_of(id);
        let tg = TimelineGame { cap: &tl.cap, irelia: tl.irelia, opponent: tl.opponent, team_of: &team_of, price_of: &price_of };
        let fs = frame_states(&tg);
        let mut fr = Vec::with_capacity(fs.len() * N_FEATURES);
        for st in &fs {
            fr.extend(state_features(st).iter().map(|v| *v as f32));
        }
        frames = Some(fr);
        let ft: Vec<f64> = tl.cap.t.iter().copied().filter(|t| *t >= 60_000.0).collect();
        // The effective window (2.2.0): when each purchase left the inventory, and the frame at or just before then.
        let lives = purchase_lives(&tl.cap, tl.irelia);
        let end_of = |end: Option<f64>, with_x: bool| -> Option<WindowEnd> {
            let t = end?;
            let frame = frame_at_or_before(&ft, t).filter(|k| *k < fs.len());
            Some(WindowEnd { t, frame, x: if with_x { frame.map(|k| Box::new(state_features(&fs[k]))) } else { None } })
        };
        item_ends = ends_of(&lives, &item_times).into_iter().map(|e| end_of(e, true)).collect();
        boots_end = boots_time.and_then(|b| end_of(ends_of(&lives, &[b])[0], true));
        let bought: Vec<(f64, f64)> = game.purchases.iter().map(|(id, ts)| (*id as f64, *ts)).collect();
        purchase_ends = ends_of(&lives, &bought).into_iter().map(|e| end_of(e, false)).collect();
        frame_t = Some(ft);
        let mut decisions = item_times.clone();
        if let Some(b) = boots_time {
            decisions.push(b);
        }
        let moments = moments_at(&tl, &decisions.iter().map(|d| (d.1 - 1.0).max(0.0)).collect::<Vec<_>>(), profiles, data);
        let made: Vec<ItemChoice> =
            decisions.iter().zip(moments.iter()).map(|(d, m)| ItemChoice { item: d.0, t: d.1, x: Some(Box::new(m.x)), live: m.live }).collect();
        choices = made[..item_times.len()].to_vec();
        boots = if boots_time.is_some() { made.last().cloned() } else { None };
        let item_of = |id: f64| data.items.get(&(id as i64));
        let decided = swap_decisions(&tl.cap, tl.irelia, &item_of, &SWAP_DEFAULTS);
        if !decided.is_empty() {
            let mut times: Vec<f64> = Vec::new();
            for d in &decided {
                if !times.contains(&d.t) {
                    times.push(d.t);
                }
            }
            let at = moments_at(&tl, &times.iter().map(|t| (t - 1.0).max(0.0)).collect::<Vec<_>>(), profiles, data);
            swaps = decided
                .iter()
                .map(|d| {
                    let k = times.iter().position(|t| *t == d.t).unwrap();
                    SwapChoice { item: d.option, t: d.t, held: d.held, w: d.w, x: Some(Box::new(at[k].x)), live: at[k].live, p: None }
                })
                .collect();
        }
        lane10 = lane_diff(&tl.cap, tl.irelia, tl.opponent, 10.0).map(|x| x.0);
        lane15 = lane_diff(&tl.cap, tl.irelia, tl.opponent, 15.0).map(|x| x.0);
    }
    let (max_first, early) = skill_choices(&row.skill_order);
    let mut summ = [row.summoner1, row.summoner2];
    summ.sort();
    Some(PreparedGame {
        match_id: row.match_id.clone(),
        platform: row.platform.clone().unwrap_or_default(),
        patch: row.patch.clone(),
        created: row.created_at as f64,
        pilot: row.puuid.clone(),
        source: row.pilot_source.clone().unwrap_or_else(|| "otp".into()),
        tier: row.pilot_tier.clone(),
        lp: row.pilot_lp.map(|x| x as f64),
        mastery: row.pilot_mastery.map(|x| x as f64),
        lane,
        dg: None,
        ix: usize::MAX,
        win: if row.win != 0 { 1.0 } else { 0.0 },
        opponent,
        enemies: enemy_ps.iter().map(|p| p.champion_id).collect(),
        allies: ally_ps.iter().map(|p| p.champion_id).collect(),
        draft,
        primary: row.primary_style,
        sub: row.sub_style,
        perks: game.perks.clone(),
        shards: game.stat_perks.clone(),
        summoners: format!("{},{}", summ[0], summ[1]),
        start: game.start.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","),
        max_first,
        early,
        items: choices,
        boots: boots.filter(|b| is_tier2_boots(data.items.get(&(b.item as i64)))),
        boots_end: boots_end.filter(|_| boots_time.map(|b| is_tier2_boots(data.items.get(&(b.0 as i64)))).unwrap_or(false)),
        swaps,
        frames,
        frame_t,
        p_at: None,
        purchases: game.purchases.clone(),
        item_ends,
        purchase_ends,
        kda: [row.kills as f64, row.deaths as f64, row.assists as f64],
        duration: row.game_duration as f64,
        lane10,
        lane15,
    })
}

/// Validated games (top and mid) with their decisions and contexts, in row order.
pub fn prepare_games(rows: &[Arc<IreliaGameRow>], participants: &[ParticipantLite], data: &StaticData, profiles: &ChampionProfiles, o: &PrepareOptions) -> Vec<PreparedGame> {
    let by = by_match(participants);
    let empty: Vec<&ParticipantLite> = vec![];
    rows.iter().filter_map(|r| prepare_game(r, by.get(r.match_id.as_str()).unwrap_or(&empty), data, profiles, o)).collect()
}

pub const STATE_MAX_ROWS: usize = 60_000;

/// The frames a state model is fitted on (as f64 rows), and how many games.
pub struct StateRows {
    pub x: Vec<[f64; N_FEATURES]>,
    pub win: Vec<f64>,
    pub games: usize,
}

/// The frames of `games` to fit the state model on (never the user's own); None when there are too few.
pub fn state_rows<'a>(games: impl Iterator<Item = &'a PreparedGame> + Clone, max_rows: usize) -> Option<StateRows> {
    state_rows_by(games, max_rows, StateSample::Even)
}

/// The frames to fit on, by `sample`: today's even stride, or one frame drawn at random from each 5-minute interval of
/// each game (seeded by the game) with the even stride through those when they pass `max_rows`.
pub fn state_rows_by<'a>(games: impl Iterator<Item = &'a PreparedGame> + Clone, max_rows: usize, sample: StateSample) -> Option<StateRows> {
    if sample == StateSample::Interval {
        return interval_rows(games, max_rows);
    }
    let with: Vec<&PreparedGame> = games.filter(|g| g.frames.is_some() && g.source != "me").collect();
    let total: usize = with.iter().map(|g| g.frames.as_ref().unwrap().len() / N_FEATURES).sum();
    if with.len() < 20 || total < 200 {
        return None;
    }
    let stride = 1usize.max((total as f64 / max_rows as f64).ceil() as usize);
    let mut x = Vec::new();
    let mut win = Vec::new();
    let mut i = 0usize;
    for g in &with {
        let fr = g.frames.as_ref().unwrap();
        for f in 0..fr.len() / N_FEATURES {
            if i % stride == 0 {
                let mut row = [0.0; N_FEATURES];
                for (k, v) in fr[f * N_FEATURES..(f + 1) * N_FEATURES].iter().enumerate() {
                    row[k] = *v as f64;
                }
                x.push(row);
                win.push(g.win);
            }
            i += 1;
        }
    }
    Some(StateRows { x, win, games: with.len() })
}

fn interval_rows<'a>(games: impl Iterator<Item = &'a PreparedGame> + Clone, max_rows: usize) -> Option<StateRows> {
    let with: Vec<&PreparedGame> = games.filter(|g| g.frames.is_some() && g.source != "me").collect();
    let total: usize = with.iter().map(|g| g.frames.as_ref().unwrap().len() / N_FEATURES).sum();
    if with.len() < 20 || total < 200 {
        return None;
    }
    let picks: Vec<Vec<usize>> = with
        .iter()
        .map(|g| {
            let n = g.frames.as_ref().unwrap().len() / N_FEATURES;
            // Frames are a minute apart from 1:00 when a game has no times.
            let t: Vec<f64> = match &g.frame_t {
                Some(t) if t.len() == n => t.clone(),
                _ => (0..n).map(|k| (k + 1) as f64 * 60_000.0).collect(),
            };
            interval_frames(&t, &g.match_id, SAMPLE_SEED)
        })
        .collect();
    let sampled: usize = picks.iter().map(|p| p.len()).sum();
    let stride = 1usize.max((sampled as f64 / max_rows as f64).ceil() as usize);
    let mut x = Vec::new();
    let mut win = Vec::new();
    let mut i = 0usize;
    for (g, pick) in with.iter().zip(picks.iter()) {
        let fr = g.frames.as_ref().unwrap();
        for f in pick {
            if i % stride == 0 {
                let mut row = [0.0; N_FEATURES];
                for (k, v) in fr[f * N_FEATURES..(f + 1) * N_FEATURES].iter().enumerate() {
                    row[k] = *v as f64;
                }
                x.push(row);
                win.push(g.win);
            }
            i += 1;
        }
    }
    Some(StateRows { x, win, games: with.len() })
}

/// Fits the state model on the timelines of `games`; None when there are too few.
pub fn train_state<'a>(games: impl Iterator<Item = &'a PreparedGame> + Clone, lambda: f64, max_rows: usize) -> Result<Option<StateModel>> {
    train_state_by(games, lambda, max_rows, StateSample::Even)
}

/// The same with the frames `sample` takes.
pub fn train_state_by<'a>(games: impl Iterator<Item = &'a PreparedGame> + Clone, lambda: f64, max_rows: usize, sample: StateSample) -> Result<Option<StateModel>> {
    let Some(r) = state_rows_by(games, max_rows, sample) else { return Ok(None) };
    let rows: Vec<&[f64]> = r.x.iter().map(|x| x.as_slice()).collect();
    Ok(Some(train_state_model(&rows, &r.win, r.games as f64, lambda)?))
}

/// Win-probability-added of every decision. `expected`: each game's chance before it started (NaN keeps the rate).
pub fn score_games(games: &[Arc<PreparedGame>], model: Option<&StateModel>, expected: Option<&[f64]>) -> Vec<ScoredGame> {
    score_games_with(games, model, expected, false)
}

/// The same; `window`: an item sold or consumed into an upgrade counts until then (2.2.0's effective window).
pub fn score_games_with(games: &[Arc<PreparedGame>], model: Option<&StateModel>, expected: Option<&[f64]>, window: bool) -> Vec<ScoredGame> {
    let base = model.map(|m| m.base).unwrap_or(0.5);
    let p = |x: &Option<Box<[f64; N_FEATURES]>>| match (model, x) {
        (Some(m), Some(x)) => win_probability_of(&m.beta, &x[..]),
        _ => base,
    };
    // The chance when an item left the inventory; before the first frame, the chance before it (no change).
    let at_end = |e: Option<&Option<WindowEnd>>, before: f64| -> Option<f64> { e.and_then(|e| e.as_ref()).map(|e| if e.x.is_some() { p(&e.x) } else { before }) };
    games
        .iter()
        .enumerate()
        .map(|(i, g)| ScoredGame {
            g: g.clone(),
            wpa0: g.win - expected.and_then(|e| e.get(i).copied()).filter(|e| e.is_finite()).unwrap_or(base),
            item_wpa: g
                .items
                .iter()
                .enumerate()
                .map(|(k, c)| if window { let b = p(&c.x); item_delta(g.win, b, at_end(g.item_ends.get(k), b), true) } else { g.win - p(&c.x) })
                .collect(),
            boots_wpa: g.boots.as_ref().map(|b| if window { let pb = p(&b.x); item_delta(g.win, pb, at_end(Some(&g.boots_end), pb), true) } else { g.win - p(&b.x) }),
            swap_wpa: g.swaps.iter().map(|c| g.win - p(&c.x)).collect(),
            item_w0: g.items.iter().map(|c| p(&c.x)).collect(),
            boots_w0: g.boots.as_ref().map(|b| p(&b.x)),
        })
        .collect()
}
