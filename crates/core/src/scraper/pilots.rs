//! Port of `server/scraper/pilots.ts`: who counts as an Irelia player, and how much each next request should yield.

use crate::db::{PilotUpdate, PlayerRow};
use crate::riot::client::MasteryDto;
use crate::scope::ScrapeScope;

pub fn classify_pilot(top: &[MasteryDto], irelia: Option<&MasteryDto>, champion_id: i64, scope: &ScrapeScope) -> PilotUpdate {
    let idx = top.iter().position(|m| m.champion_id == champion_id);
    let entry = match idx {
        Some(i) => Some(&top[i]),
        None => irelia.filter(|m| m.champion_id == champion_id),
    };
    let points = entry.map(|e| e.champion_points).unwrap_or(0);
    let rank = idx.map(|i| i as i64 + 1);
    let class = if rank == Some(1) && points as f64 >= scope.otp_min_mastery_points {
        "otp"
    } else if points as f64 >= scope.main_min_mastery_points {
        "main"
    } else {
        "none"
    };
    PilotUpdate { pilot_class: class.into(), irelia_points: points, irelia_last_play: entry.and_then(|e| e.last_play_time), irelia_rank: rank }
}

/// Played Irelia since `since`. Unknown activity counts as active.
pub fn is_active(p: &PlayerRow, since: i64) -> bool {
    p.irelia_last_play.map(|l| l >= since).unwrap_or(true)
}

fn prior_share(source: &str) -> f64 {
    match source {
        "otp" => 0.7,
        "main" => 0.35,
        "me" => 0.5,
        _ => 0.3,
    }
}
const PRIOR_WEIGHT: f64 = 5.0;
const PRIOR_GAMES_PER_HOUR: f64 = 0.25;
const FIRST_SCAN_MATCHES: f64 = 30.0;

pub fn irelia_share(p: &PlayerRow, source: Option<&str>) -> f64 {
    let source = source.or(p.pilot_class.as_deref()).unwrap_or("otp");
    let prior = prior_share(source);
    (p.history_irelia as f64 + prior * PRIOR_WEIGHT) / (p.history_matches as f64 + PRIOR_WEIGHT)
}

pub fn match_score(p: f64) -> f64 {
    p / (1.0 + p)
}

pub const HARVEST_SCORE: f64 = 1.0;
pub const OWN_GAMES_SCORE: f64 = 2.0;

pub fn history_score(p: &PlayerRow, now: i64, max_matches: f64, source: Option<&str>) -> f64 {
    let share = irelia_share(p, source);
    let g = match p.last_match_scan.filter(|s| *s != 0) {
        Some(last) => f64::min(max_matches, f64::max(0.5, ((now - last) as f64 / 3_600_000.0) * PRIOR_GAMES_PER_HOUR)),
        None => f64::min(max_matches, FIRST_SCAN_MATCHES),
    };
    (share * g) / (1.0 + g * (1.0 + share))
}
