//! Port of `server/scraper/ingest.ts`: one match with everything v3 keeps. Fetching writes nothing; storing is one
//! transaction (fetch_match lives with the Riot client in scraper/fetch.rs).

use crate::capture::{capture_timeline, lane_opponents, leavers, MatchQuality, TimelineCapture, CAPTURE_VERSION};
use crate::db::{Db, IreliaGameRow, MatchRow, ParticipantRow, PlayerRow};
use crate::engine::parse_match::extract_irelia_game;
use anyhow::Result;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct GameCapture {
    pub row: IreliaGameRow,
    pub timeline: TimelineCapture,
    pub leaver_ids: Vec<i64>,
}

/// The Irelia row of a stored game, with its v3 fields.
pub fn capture_game(m: &Value, timeline: &Value, me: &Value, source: &str, pilot: Option<&PlayerRow>) -> GameCapture {
    let cap = capture_timeline(timeline);
    let base = extract_irelia_game(m, timeline, me);
    let opp = lane_opponents(m, Some(&cap), me);
    let row = IreliaGameRow {
        opponent_champion_id: opp.effective,
        participant_id: me["participantId"].as_i64(),
        position_opponent_id: opp.position,
        timeline_opponent_id: opp.timeline,
        pilot_source: Some(source.into()),
        pilot_tier: pilot.map(|p| p.tier.clone()),
        pilot_lp: pilot.map(|p| p.league_points),
        pilot_mastery: pilot.and_then(|p| p.irelia_points.or(p.irelia_mastery)),
        pilot_last_play: pilot.and_then(|p| p.irelia_last_play),
        pilot_top1: pilot.and_then(|p| {
            if p.irelia_rank == Some(1) || p.is_otp == Some(1) {
                Some(1)
            } else if p.irelia_rank.map(|r| r != 0).unwrap_or(false) {
                Some(0)
            } else {
                None
            }
        }),
        ..base
    };
    GameCapture { leaver_ids: leavers(&cap), row, timeline: cap }
}

/// What fetch_match found (ingest.ts Fetched).
#[derive(Clone, Debug)]
pub enum Fetched {
    Known,
    Missing,
    Fetched(Box<FetchedMatch>),
}

#[derive(Clone, Debug)]
pub struct FetchedMatch {
    pub row: MatchRow,
    pub quality: MatchQuality,
    pub participants: Vec<ParticipantRow>,
    pub stale: bool,
    pub game: Option<GameCapture>,
    pub pilot_played_irelia: bool,
    pub candidate: Option<(String, String)>,
    pub pilot_puuid: Option<String>,
    pub source: String,
}

/// Writes a fetched match in one transaction. Returns 'irelia' | 'other' | 'stale' | 'remake' | 'missing' | 'known'.
pub fn store_match(db: &Db, id: &str, fetched: &Fetched) -> Result<&'static str> {
    let f = match fetched {
        Fetched::Known => return Ok("known"),
        Fetched::Missing => return Ok("missing"),
        Fetched::Fetched(f) => f,
    };
    db.tx(|| {
        if !db.insert_match(&f.row)? {
            return Ok("known");
        }
        db.insert_participants(id, &f.participants)?;
        db.set_quality(id, f.quality.remake, f.quality.end_result.as_deref(), CAPTURE_VERSION)?;
        if let Some(g) = &f.game {
            store_game(db, g)?;
        }
        if let Some((puuid, platform)) = &f.candidate {
            db.add_candidate(puuid, platform, "harvest")?;
        }
        if let Some(p) = &f.pilot_puuid {
            if f.source != "harvested" && !f.stale {
                db.count_history(p, 1, if f.pilot_played_irelia && f.game.is_some() { 1 } else { 0 })?;
            }
        }
        Ok(if f.game.is_some() {
            "irelia"
        } else if f.quality.remake {
            "remake"
        } else if f.stale {
            "stale"
        } else {
            "other"
        })
    })
}

/// The Irelia game, its timeline and the leaver flag (inside the caller's transaction).
pub fn store_game(db: &Db, g: &GameCapture) -> Result<()> {
    db.insert_game(&g.row)?;
    db.insert_timeline(&g.row.match_id, g.timeline.interval as i64, &g.timeline.to_json_string())?;
    db.set_leaver(&g.row.match_id, !g.leaver_ids.is_empty())?;
    db.set_has_irelia(&g.row.match_id)?;
    Ok(())
}
