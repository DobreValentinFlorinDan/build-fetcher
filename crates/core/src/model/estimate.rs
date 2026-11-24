//! Port of `server/model/estimate.ts`: one decision's options scored by their mean win-probability-added, shrunk in
//! layers (both lanes → this lane → similar drafts → this exact matchup). The sums are added in the TS order, so the
//! estimates are the same to the last bit.

use super::context::{kernel, kernel_at, Contexts};
use super::logistic::js_max;
use crate::engine::recommend::{js_order, or_else};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// An observation's context: none, its own array, or a row of a model's contexts.
#[derive(Clone, Debug)]
pub enum Z {
    None,
    Own(Arc<Vec<f64>>),
    Row(Arc<Contexts>, usize),
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub option: Arc<str>,
    pub wpa: f64,
    pub win: f64,
    /// The win chance just before this decision (the state model's, as its own value: never worked out from `wpa`).
    /// NaN when not known (pre-game choices, swaps).
    pub start_win: f64,
    pub w: f64,
    pub lane: &'static str,
    pub opponent: Option<i64>,
    pub pilot: Arc<str>,
    pub z: Z,
    pub row: Option<usize>,
    pub age: Option<f64>,
}

/// A decision's observations, indexed per lane on first use (the TS WeakMap on the array).
pub struct Obs {
    pub list: Vec<Observation>,
    idx: [OnceLock<LaneIndex>; 2],
}

pub type ObsRef = Arc<Obs>;

impl Obs {
    pub fn new(list: Vec<Observation>) -> ObsRef {
        Arc::new(Obs { list, idx: [OnceLock::new(), OnceLock::new()] })
    }
    pub fn len(&self) -> usize {
        self.list.len()
    }
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
    fn key(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize
    }
    fn lane_index(&self, lane: &str) -> &LaneIndex {
        let slot = if lane == "TOP" { 0 } else { 1 };
        self.idx[slot].get_or_init(|| build_lane_index(&self.list, lane))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EstimateOptions {
    pub h: f64,
    pub k: f64,
    pub min_games: f64,
    pub min_share: f64,
}

pub const ESTIMATE_DEFAULTS: EstimateOptions = EstimateOptions { h: 0.7, k: 100.0, min_games: 5.0, min_share: 0.03 };

#[derive(Clone, Debug)]
pub struct Sims {
    pub h: f64,
    pub values: Arc<Vec<f64>>,
    /// Grouped drafts: each game's group and each group's similarity.
    pub groups: Option<(Arc<Vec<i32>>, Arc<Vec<f64>>)>,
}

/// Sums kept for one draft at one bandwidth (Query.memo), and its similarities (draft.ts memoSims).
#[derive(Default)]
pub struct Memo {
    pub sums: HashMap<usize, Arc<Sums>>,
    pub sims: Option<Arc<Vec<f64>>>,
    pub group_sims: Option<Arc<Vec<f64>>>,
    /// Keeps the observation lists alive while their address keys the sums.
    pub keep: Vec<ObsRef>,
}

#[derive(Clone)]
pub struct Query<'a> {
    pub lane: &'static str,
    pub opponent: Option<i64>,
    pub z: Vec<f64>,
    pub sims: Option<Sims>,
    pub memo: Option<&'a RefCell<Memo>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OptionEstimate {
    pub option: Arc<str>,
    pub theta: f64,
    pub low: f64,
    pub high: f64,
    pub lane_theta: f64,
    pub context_theta: f64,
    pub n_eff: f64,
    pub matchup_games: f64,
    pub games: f64,
    pub pilots: f64,
    pub win_rate: f64,
    /// Mean win chance just before the purchase over the same weighted games as `win_rate` (None when not known).
    pub start_win: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct DecisionEstimate {
    pub options: Vec<OptionEstimate>,
    pub n_eff: f64,
    pub sigma: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Acc {
    pub sw: f64,
    pub sw2: f64,
    pub swx: f64,
    pub swin: f64,
    /// Σ weight · win chance before the decision (beside `swin`, never mixed into the others).
    pub sw0: f64,
    pub n: f64,
}

impl Acc {
    #[inline]
    pub fn add(&mut self, w: f64, x: f64, win: f64, k: f64) {
        // (`w <= 0` is false for NaN in JS as here: a NaN weight is added, as there.)
        if w <= 0.0 || k <= 0.0 {
            return;
        }
        let wk = w * k;
        self.sw += wk;
        self.sw2 += w * wk;
        self.swx += wk * x;
        self.swin += wk * win;
        self.n += 1.0;
    }
    /// Adds a decision's starting win chance under the same guard and weight as `add`.
    #[inline]
    pub fn add_start(&mut self, w: f64, k: f64, start: f64) {
        if w <= 0.0 || k <= 0.0 {
            return;
        }
        self.sw0 += w * k * start;
    }
    pub fn mean(&self) -> f64 {
        if self.sw > 0.0 {
            self.swx / self.sw
        } else {
            0.0
        }
    }
    pub fn n_eff(&self) -> f64 {
        if self.sw2 > 0.0 {
            (self.sw * self.sw) / self.sw2
        } else {
            0.0
        }
    }
}

#[inline]
fn towards(n_eff: f64, mean: f64, prior: f64, k: f64) -> f64 {
    (n_eff * mean + k * prior) / (n_eff + k)
}

fn any_known(z: &[f64]) -> bool {
    z.iter().any(|x| !x.is_nan())
}

/// Per option: weighted sums of one opponent's games, how many, and whether any.
#[derive(Clone, Debug, Default)]
pub struct MatchupSums {
    pub sw: Vec<f64>,
    pub sw2: Vec<f64>,
    pub swx: Vec<f64>,
    pub swin: Vec<f64>,
    pub n: Vec<i32>,
    pub seen: Vec<u8>,
}

impl MatchupSums {
    fn empty(n: usize) -> MatchupSums {
        MatchupSums { sw: vec![0.0; n], sw2: vec![0.0; n], swx: vec![0.0; n], swin: vec![0.0; n], n: vec![0; n], seen: vec![0; n] }
    }
}

#[derive(Clone, Debug)]
pub struct Cells {
    of: Arc<Vec<i32>>,
    cell_start: Vec<usize>,
    cell_group: Vec<i32>,
    sw: Vec<f64>,
    sw2: Vec<f64>,
    swx: Vec<f64>,
    swin: Vec<f64>,
    sw0: Vec<f64>,
    matchup: Vec<usize>,
    by_opponent: HashMap<i64, (usize, usize)>,
    tw: Vec<f64>,
    tw2: Vec<f64>,
}

pub struct LaneIndex {
    pub options: Vec<Arc<str>>,
    all: Vec<Acc>,
    lane: Vec<Acc>,
    pilots: Vec<f64>,
    opt: Vec<usize>,
    w: Vec<f64>,
    wpa: Vec<f64>,
    win: Vec<f64>,
    w0: Vec<f64>,
    opponent: Vec<Option<i64>>,
    z: Vec<Z>,
    row: Vec<i64>,
    all_rows: bool,
    lane_weight: f64,
    sq: f64,
    sq_w: f64,
    tsw: f64,
    tsw2: f64,
    by_opponent: Mutex<HashMap<i64, Arc<MatchupSums>>>,
    // grouped by option
    start: Vec<usize>,
    gw: Vec<f64>,
    gwpa: Vec<f64>,
    gwin: Vec<f64>,
    gw0: Vec<f64>,
    gopp: Vec<i64>,
    grow: Vec<i64>,
    cells: Mutex<Option<Arc<Cells>>>,
}

fn build_lane_index(obs: &[Observation], lane: &str) -> LaneIndex {
    let mut all_of: indexmap::IndexMap<Arc<str>, Acc> = indexmap::IndexMap::new();
    for ob in obs {
        all_of.entry(ob.option.clone()).or_default().add(ob.w, ob.wpa, ob.win, 1.0);
    }
    let in_lane: Vec<&Observation> = obs.iter().filter(|o| o.lane == lane).collect();
    let len = in_lane.len();
    let mut at: HashMap<Arc<str>, usize> = HashMap::new();
    let mut idx = LaneIndex {
        options: vec![],
        all: vec![],
        lane: vec![],
        pilots: vec![],
        opt: vec![0; len],
        w: vec![0.0; len],
        wpa: vec![0.0; len],
        win: vec![0.0; len],
        w0: vec![0.0; len],
        opponent: vec![None; len],
        z: Vec::with_capacity(len),
        row: vec![-1; len],
        all_rows: true,
        lane_weight: 0.0,
        sq: 0.0,
        sq_w: 0.0,
        tsw: 0.0,
        tsw2: 0.0,
        by_opponent: Mutex::new(HashMap::new()),
        start: vec![],
        gw: vec![],
        gwpa: vec![],
        gwin: vec![],
        gw0: vec![],
        gopp: vec![],
        grow: vec![],
        cells: Mutex::new(None),
    };
    let mut pilots: Vec<std::collections::HashSet<Arc<str>>> = Vec::new();
    for (j, ob) in in_lane.iter().enumerate() {
        let k = match at.get(&ob.option) {
            Some(k) => *k,
            None => {
                let k = idx.options.len();
                at.insert(ob.option.clone(), k);
                idx.options.push(ob.option.clone());
                idx.all.push(all_of[&ob.option]);
                idx.lane.push(Acc::default());
                pilots.push(Default::default());
                k
            }
        };
        idx.lane[k].add(ob.w, ob.wpa, ob.win, 1.0);
        idx.lane[k].add_start(ob.w, 1.0, ob.start_win);
        pilots[k].insert(ob.pilot.clone());
        idx.opt[j] = k;
        idx.w[j] = ob.w;
        idx.wpa[j] = ob.wpa;
        idx.win[j] = ob.win;
        idx.w0[j] = ob.start_win;
        idx.opponent[j] = ob.opponent;
        idx.z.push(ob.z.clone());
        idx.row[j] = ob.row.map(|r| r as i64).unwrap_or(-1);
        if idx.row[j] < 0 {
            idx.all_rows = false;
        }
        let w = ob.w;
        let wpa = ob.wpa;
        idx.lane_weight += w;
        idx.sq += w * wpa * wpa;
        idx.sq_w += w;
        if w > 0.0 {
            idx.tsw += w;
            idx.tsw2 += w * w;
        }
    }
    idx.pilots = pilots.iter().map(|p| p.len() as f64).collect();
    let n = idx.options.len();
    let mut start = vec![0usize; n + 1];
    for j in 0..len {
        start[idx.opt[j] + 1] += 1;
    }
    for k in 0..n {
        start[k + 1] += start[k];
    }
    let mut next = start[..n].to_vec();
    idx.gw = vec![0.0; len];
    idx.gwpa = vec![0.0; len];
    idx.gwin = vec![0.0; len];
    idx.gw0 = vec![0.0; len];
    idx.gopp = vec![0; len];
    idx.grow = vec![0; len];
    for j in 0..len {
        let a = next[idx.opt[j]];
        next[idx.opt[j]] += 1;
        idx.gw[a] = idx.w[j];
        idx.gwpa[a] = idx.wpa[j];
        idx.gwin[a] = idx.win[j];
        idx.gw0[a] = idx.w0[j];
        idx.gopp[a] = idx.opponent[j].unwrap_or(-1);
        idx.grow[a] = idx.row[j];
    }
    idx.start = start;
    idx
}

fn cells_of(idx: &LaneIndex, of: &Arc<Vec<i32>>) -> Arc<Cells> {
    let mut slot = idx.cells.lock();
    if let Some(c) = slot.as_ref() {
        if Arc::ptr_eq(&c.of, of) {
            return c.clone();
        }
    }
    let n = idx.options.len();
    let mut groups = 0usize;
    for p in 0..idx.grow.len() {
        groups = groups.max((of[idx.grow[p] as usize] + 1) as usize);
    }
    let mut cell_start = vec![0usize; n + 1];
    let (mut cell_group, mut sw, mut sw2, mut swx, mut swin) = (vec![], vec![], vec![], vec![], vec![]);
    let mut sw0: Vec<f64> = vec![];
    let mut at = vec![-1i64; groups];
    let mut tw = vec![0.0; groups];
    let mut tw2 = vec![0.0; groups];
    let mut keys: Vec<i64> = vec![];
    let mut pos: Vec<Vec<usize>> = vec![];
    let mut key_at: HashMap<i64, usize> = HashMap::new();
    for k in 0..n {
        let mut touched: Vec<usize> = vec![];
        for p in idx.start[k]..idx.start[k + 1] {
            let opp = idx.gopp[p];
            if opp >= 0 {
                let key = k as i64 * 65536 + opp;
                let x = *key_at.entry(key).or_insert_with(|| {
                    keys.push(key);
                    pos.push(vec![]);
                    keys.len() - 1
                });
                pos[x].push(p);
            }
            let w = idx.gw[p];
            if w <= 0.0 {
                continue;
            }
            let g = of[idx.grow[p] as usize] as usize;
            let mut c = at[g];
            if c < 0 {
                c = cell_group.len() as i64;
                at[g] = c;
                touched.push(g);
                cell_group.push(g as i32);
                sw.push(0.0);
                sw2.push(0.0);
                swx.push(0.0);
                swin.push(0.0);
                sw0.push(0.0);
            }
            let c = c as usize;
            sw[c] += w;
            sw2[c] += w * w;
            swx[c] += w * idx.gwpa[p];
            swin[c] += w * idx.gwin[p];
            sw0[c] += w * idx.gw0[p];
            tw[g] += w;
            tw2[g] += w * w;
        }
        for g in touched {
            at[g] = -1;
        }
        cell_start[k + 1] = cell_group.len();
    }
    let mut matchup = Vec::with_capacity(pos.iter().map(|l| l.len()).sum());
    let mut by_opponent = HashMap::new();
    for (x, key) in keys.iter().enumerate() {
        let from = matchup.len();
        matchup.extend_from_slice(&pos[x]);
        by_opponent.insert(*key, (from, matchup.len()));
    }
    let c = Arc::new(Cells { of: of.clone(), cell_start, cell_group, sw, sw2, swx, swin, sw0, matchup, by_opponent, tw, tw2 });
    *slot = Some(c.clone());
    c
}

/// The sums a query adds up.
#[derive(Clone, Debug)]
pub struct Sums {
    pub c_sw: Vec<f64>,
    pub c_sw2: Vec<f64>,
    pub c_swx: Vec<f64>,
    pub c_swin: Vec<f64>,
    pub c_sw0: Vec<f64>,
    pub m: Arc<MatchupSums>,
    pub tsw: f64,
    pub tsw2: f64,
    pub lane_weight: f64,
    pub sq: f64,
    pub sq_w: f64,
}

fn cell_sums(idx: &LaneIndex, of: &Arc<Vec<i32>>, gs: &[f64], opponent: i64) -> Sums {
    let cells = cells_of(idx, of);
    let n = idx.options.len();
    let mut s = Sums {
        c_sw: vec![0.0; n],
        c_sw2: vec![0.0; n],
        c_swx: vec![0.0; n],
        c_swin: vec![0.0; n],
        c_sw0: vec![0.0; n],
        m: Arc::new(MatchupSums::empty(n)),
        tsw: 0.0,
        tsw2: 0.0,
        lane_weight: idx.lane_weight,
        sq: idx.sq,
        sq_w: idx.sq_w,
    };
    let mut m = MatchupSums::empty(n);
    for k in 0..n {
        let (mut sw, mut sw2, mut swx, mut swin, mut sw0) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for c in cells.cell_start[k]..cells.cell_start[k + 1] {
            let sim = gs[cells.cell_group[c] as usize];
            if sim <= 0.0 {
                continue;
            }
            sw += sim * cells.sw[c];
            sw2 += sim * cells.sw2[c];
            swx += sim * cells.swx[c];
            swin += sim * cells.swin[c];
            sw0 += sim * cells.sw0[c];
        }
        s.c_sw[k] = sw;
        s.c_sw2[k] = sw2;
        s.c_swx[k] = swx;
        s.c_swin[k] = swin;
        s.c_sw0[k] = sw0;
        let (mut ms, mut ms2, mut msx, mut mswn, mut mm, mut seen) = (0.0, 0.0, 0.0, 0.0, 0, 0u8);
        if opponent >= 0 {
            if let Some((from, to)) = cells.by_opponent.get(&(k as i64 * 65536 + opponent)) {
                seen = 1;
                for x in *from..*to {
                    let p = cells.matchup[x];
                    let w = idx.gw[p];
                    let sim = gs[of[idx.grow[p] as usize] as usize];
                    if w <= 0.0 || sim <= 0.0 {
                        continue;
                    }
                    let wk = w * sim;
                    ms += wk;
                    ms2 += w * wk;
                    msx += wk * idx.gwpa[p];
                    mswn += wk * idx.gwin[p];
                    mm += 1;
                }
            }
        }
        m.sw[k] = ms;
        m.sw2[k] = ms2;
        m.swx[k] = msx;
        m.swin[k] = mswn;
        m.n[k] = mm;
        m.seen[k] = seen;
    }
    let (mut tsw, mut tsw2) = (0.0, 0.0);
    for g in 0..cells.tw.len() {
        let sim = gs[g];
        if sim > 0.0 {
            tsw += sim * cells.tw[g];
            tsw2 += sim * cells.tw2[g];
        }
    }
    s.m = Arc::new(m);
    s.tsw = tsw;
    s.tsw2 = tsw2;
    s
}

fn opponent_sums(idx: &LaneIndex, opponent: i64) -> Arc<MatchupSums> {
    if let Some(m) = idx.by_opponent.lock().get(&opponent) {
        return m.clone();
    }
    let mut m = MatchupSums::empty(idx.options.len());
    for j in 0..idx.opt.len() {
        if idx.opponent[j] != Some(opponent) {
            continue;
        }
        let k = idx.opt[j];
        m.seen[k] = 1;
        let w = idx.w[j];
        if w <= 0.0 {
            continue;
        }
        let wk = w * 1.0;
        m.sw[k] += wk;
        m.sw2[k] += w * wk;
        m.swx[k] += wk * idx.wpa[j];
        m.swin[k] += wk * idx.win[j];
        m.n[k] += 1;
    }
    let m = Arc::new(m);
    idx.by_opponent.lock().insert(opponent, m.clone());
    m
}

/// The loop every held-out game runs for each decision (option by option over the grouped arrays).
fn grouped_loop(idx: &LaneIndex, sims: &[f64], opponent: i64, s: &mut Sums, m: &mut MatchupSums) {
    let start = &idx.start;
    for k in 0..start.len().saturating_sub(1) {
        let (mut sw, mut sw2, mut swx, mut swin, mut ms, mut ms2, mut msx, mut mswn, mut n, mut seen) =
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0i32, 0u8);
        let mut sw0 = 0.0;
        for p in start[k]..start[k + 1] {
            let w = idx.gw[p];
            let sim = sims[idx.grow[p] as usize];
            let matchup = idx.gopp[p] == opponent;
            if matchup {
                seen = 1;
            }
            if w <= 0.0 || sim <= 0.0 {
                continue;
            }
            let wk = w * sim;
            let wpa = idx.gwpa[p];
            let win = idx.gwin[p];
            sw += wk;
            sw2 += w * wk;
            swx += wk * wpa;
            swin += wk * win;
            sw0 += wk * idx.gw0[p];
            if matchup {
                ms += wk;
                ms2 += w * wk;
                msx += wk * wpa;
                mswn += wk * win;
                n += 1;
            }
        }
        s.c_sw[k] = sw;
        s.c_sw2[k] = sw2;
        s.c_swx[k] = swx;
        s.c_swin[k] = swin;
        s.c_sw0[k] = sw0;
        m.sw[k] = ms;
        m.sw2[k] = ms2;
        m.swx[k] = msx;
        m.swin[k] = mswn;
        m.n[k] = n;
        m.seen[k] = seen;
    }
}

fn query_sums(idx: &LaneIndex, q: &Query, o: &EstimateOptions, has_context: bool, sims: Option<&Sims>, use_: Option<&[bool]>, dim: Option<usize>) -> Sums {
    let n = idx.options.len();
    if let (Some(s), true, true, None) = (sims, has_context, idx.all_rows, use_) {
        if let Some((of, gs)) = &s.groups {
            return cell_sums(idx, of, gs, q.opponent.unwrap_or(-2));
        }
    }
    if !has_context && use_.is_none() {
        let m = match q.opponent {
            Some(op) if op >= 0 => opponent_sums(idx, op),
            _ => Arc::new(MatchupSums::empty(n)),
        };
        return Sums {
            c_sw: idx.lane.iter().map(|a| a.sw).collect(),
            c_sw2: idx.lane.iter().map(|a| a.sw2).collect(),
            c_swx: idx.lane.iter().map(|a| a.swx).collect(),
            c_swin: idx.lane.iter().map(|a| a.swin).collect(),
            c_sw0: idx.lane.iter().map(|a| a.sw0).collect(),
            m,
            tsw: idx.tsw,
            tsw2: idx.tsw2,
            lane_weight: idx.lane_weight,
            sq: idx.sq,
            sq_w: idx.sq_w,
        };
    }
    let mut s = Sums {
        c_sw: vec![0.0; n],
        c_sw2: vec![0.0; n],
        c_swx: vec![0.0; n],
        c_swin: vec![0.0; n],
        c_sw0: vec![0.0; n],
        m: Arc::new(MatchupSums::empty(0)),
        tsw: 0.0,
        tsw2: 0.0,
        lane_weight: idx.lane_weight,
        sq: idx.sq,
        sq_w: idx.sq_w,
    };
    let mut m = MatchupSums::empty(n);
    if let (Some(sm), true, None) = (sims, idx.all_rows, use_) {
        let opponent = q.opponent.unwrap_or(-2);
        grouped_loop(idx, &sm.values, opponent, &mut s, &mut m);
        // The context total over every option, in observation order.
        let (mut tsw, mut tsw2) = (0.0, 0.0);
        for j in 0..idx.w.len() {
            let w = idx.w[j];
            let sim = sm.values[idx.row[j] as usize];
            if w <= 0.0 || sim <= 0.0 {
                continue;
            }
            let wk = w * sim;
            tsw += wk;
            tsw2 += w * wk;
        }
        s.tsw = tsw;
        s.tsw2 = tsw2;
        s.m = Arc::new(m);
        return s;
    }
    let (mut lane_weight, mut sq, mut sq_w, mut tsw, mut tsw2) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for j in 0..idx.opt.len() {
        let k = idx.opt[j];
        if let Some(u) = use_ {
            if !u[k] {
                continue;
            }
        }
        let w = idx.w[j];
        let wpa = idx.wpa[j];
        lane_weight += w;
        sq += w * wpa * wpa;
        sq_w += w;
        let sim = if !has_context {
            1.0
        } else if let (Some(sm), true) = (sims, idx.row[j] >= 0) {
            sm.values[idx.row[j] as usize]
        } else {
            match &idx.z[j] {
                Z::Row(c, r) => kernel_at(c, *r, &q.z, o.h, dim),
                Z::Own(v) => kernel(v, &q.z, o.h, dim),
                Z::None => kernel(&[], &q.z, o.h, dim),
            }
        };
        let matchup = q.opponent.is_some() && idx.opponent[j] == q.opponent;
        if matchup {
            m.seen[k] = 1;
        }
        if w <= 0.0 || sim <= 0.0 {
            continue;
        }
        let wk = w * sim;
        let win = idx.win[j];
        s.c_sw[k] += wk;
        s.c_sw2[k] += w * wk;
        s.c_swx[k] += wk * wpa;
        s.c_swin[k] += wk * win;
        s.c_sw0[k] += wk * idx.w0[j];
        tsw += wk;
        tsw2 += w * wk;
        if matchup {
            m.sw[k] += wk;
            m.sw2[k] += w * wk;
            m.swx[k] += wk * wpa;
            m.swin[k] += wk * win;
            m.n[k] += 1;
        }
    }
    s.lane_weight = lane_weight;
    s.sq = sq;
    s.sq_w = sq_w;
    s.tsw = tsw;
    s.tsw2 = tsw2;
    s.m = Arc::new(m);
    s
}

/// Estimates for every option of a decision given `obs`. `only` limits the work to some options and `dim` the kernel
/// to one context dimension (the reason search).
pub fn estimate_decision(obs: &ObsRef, q: &Query, o: &EstimateOptions, only: Option<&[&str]>, dim: Option<usize>) -> DecisionEstimate {
    let has_context = o.h.is_finite() && match dim {
        Some(d) => !q.z.get(d).copied().unwrap_or(f64::NAN).is_nan(),
        None => any_known(&q.z),
    };
    let idx = obs.lane_index(q.lane);
    let sims = if has_context && dim.is_none() { q.sims.as_ref().filter(|s| s.h == o.h) } else { None };
    let n = idx.options.len();
    let use_: Option<Vec<bool>> = only.map(|set| idx.options.iter().map(|x| set.iter().any(|s| **s == **x)).collect());
    let memo_ok = q.memo.is_some() && use_.is_none() && dim.is_none() && (!has_context || sims.is_some());
    let mut sums: Option<Arc<Sums>> = None;
    if memo_ok {
        sums = q.memo.unwrap().borrow().sums.get(&obs.key()).cloned();
    }
    let sums = match sums {
        Some(s) => s,
        None => {
            let s = Arc::new(query_sums(idx, q, o, has_context, sims, use_.as_deref(), dim));
            if memo_ok {
                let mut m = q.memo.unwrap().borrow_mut();
                m.sums.insert(obs.key(), s.clone());
                m.keep.push(obs.clone());
            }
            s
        }
    };
    let sigma = js_max(0.3, if sums.sq_w > 0.0 { (sums.sq / sums.sq_w).sqrt() } else { 0.5 });
    let at = |sw: &[f64], sw2: &[f64], swx: &[f64], k: usize| -> (f64, f64) {
        let mean = if sw[k] > 0.0 { swx[k] / sw[k] } else { 0.0 };
        let n_eff = if sw2[k] > 0.0 { (sw[k] * sw[k]) / sw2[k] } else { 0.0 };
        (mean, n_eff)
    };
    let mut options: Vec<OptionEstimate> = Vec::new();
    let (mut lane_sw, mut lane_sw2) = (0.0, 0.0);
    let m = &sums.m;
    for k in 0..n {
        if let Some(u) = &use_ {
            if !u[k] {
                continue;
            }
        }
        let l = &idx.lane[k];
        lane_sw += l.sw;
        lane_sw2 += l.sw2;
        if l.n < o.min_games || l.sw < o.min_share * sums.lane_weight {
            continue;
        }
        let a = &idx.all[k];
        let theta_all = towards(a.n_eff(), a.mean(), 0.0, o.k);
        let lane_theta = towards(l.n_eff(), l.mean(), theta_all, o.k);
        let (ck_mean, ck_n) = at(&sums.c_sw, &sums.c_sw2, &sums.c_swx, k);
        let context_theta = if has_context { towards(ck_n, ck_mean, lane_theta, o.k) } else { lane_theta };
        let mk = if m.seen.get(k).copied().unwrap_or(0) != 0 { Some(at(&m.sw, &m.sw2, &m.swx, k)) } else { None };
        let theta = match mk {
            Some((mm, mn)) => towards(mn, mm, context_theta, o.k),
            None => context_theta,
        };
        let n_eff = js_max(if has_context { ck_n } else { l.n_eff() }, mk.map(|x| x.1).unwrap_or(0.0));
        let half = 1.96 * sigma / (n_eff + o.k).sqrt();
        let (wsw, wswin) = if has_context { (sums.c_sw[k], sums.c_swin[k]) } else { (l.sw, l.swin) };
        let wsw0 = if has_context { sums.c_sw0[k] } else { l.sw0 };
        options.push(OptionEstimate {
            option: idx.options[k].clone(),
            theta,
            low: theta - half,
            high: theta + half,
            lane_theta,
            context_theta,
            n_eff: if has_context { ck_n } else { l.n_eff() },
            matchup_games: if mk.is_some() { m.n[k] as f64 } else { 0.0 },
            games: l.n,
            pilots: idx.pilots[k],
            win_rate: if wsw > 0.0 { wswin / wsw } else { 0.0 },
            start_win: Some(wsw0 / wsw).filter(|x| wsw > 0.0 && x.is_finite()),
        });
    }
    options.sort_by(|a, b| js_order(or_else(b.theta - a.theta, || b.games - a.games)));
    let total = if sums.tsw2 > 0.0 { (sums.tsw * sums.tsw) / sums.tsw2 } else { 0.0 };
    DecisionEstimate {
        options,
        n_eff: if has_context { total } else { (lane_sw * lane_sw) / js_max(1e-9, lane_sw2) },
        sigma,
    }
}
