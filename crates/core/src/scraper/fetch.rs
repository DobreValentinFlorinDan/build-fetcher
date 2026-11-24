//! Port of `server/scraper/ingest.ts` fetchMatch / fetchHarvested: fetching one match (cache first), plus its
//! timeline when an Irelia game in it will be stored. Fetching writes nothing.

use super::ingest::{capture_game, Fetched, FetchedMatch, GameCapture};
use crate::capture::{match_quality, participant_rows};
use crate::db::{Db, MatchRow};
use crate::ddragon::{compare_patch, to_patch};
use crate::patches::{is_stale, RegionPatch};
use crate::riot::client::RiotApi;
use crate::riot::rate_limiter::Signal;
use anyhow::Result;
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;

const HARVEST_POSITIONS: [&str; 2] = ["TOP", "MIDDLE"];

/// What the ingest reads besides the Riot API.
pub struct IngestEnv<'a, A: RiotApi> {
    pub db: &'a Arc<Mutex<Db>>,
    pub riot: &'a A,
    pub champion_id: i64,
    pub region: &'a (dyn Fn(&str) -> RegionPatch + Send + Sync),
    pub harvest: bool,
    pub me_puuid: Option<String>,
}

/// Whose history listed the match.
#[derive(Clone, Debug)]
pub struct IngestContext {
    pub pilot_puuid: Option<String>,
    pub source: String,
    pub keep_previous_patch: bool,
}

fn known_pilot_class(db: &Db, puuid: &str) -> Option<String> {
    db.player(puuid).and_then(|p| p.pilot_class).filter(|c| c == "otp" || c == "main")
}

fn participants(m: &Value) -> Vec<Value> {
    m["info"]["participants"].as_array().cloned().unwrap_or_default()
}

pub async fn fetch_match<A: RiotApi>(env: &IngestEnv<'_, A>, platform: &str, id: &str, ctx: &IngestContext, signal: &Signal) -> Result<Fetched> {
    if env.db.lock().has_match(id) {
        return Ok(Fetched::Known);
    }
    let Some(m) = env.riot.get_match(platform, id, signal).await? else { return Ok(Fetched::Missing) };
    let version = m["info"]["gameVersion"].as_str().unwrap_or("").to_string();
    let patch = to_patch(&version);
    let region = (env.region)(platform);
    let stale = is_stale(&patch, &region.patch);
    let keep = !stale || (ctx.keep_previous_patch && compare_patch(&patch, &region.previous_patch) >= 0.0);
    let quality = match_quality(&m);
    let mut row = MatchRow {
        id: id.into(),
        platform: platform.into(),
        patch,
        version,
        queue: m["info"]["queueId"].as_i64().unwrap_or(0),
        creation: m["info"]["gameCreation"].as_i64().unwrap_or(0),
        duration: m["info"]["gameDuration"].as_i64().unwrap_or(0),
        has_irelia: false,
    };
    let ps = participants(&m);
    let irelia = ps.iter().find(|p| p["championId"].as_i64() == Some(env.champion_id)).cloned();
    let irelia_puuid = irelia.as_ref().and_then(|p| p["puuid"].as_str().map(String::from));
    let mine = irelia_puuid.is_some() && irelia_puuid == ctx.pilot_puuid;
    let pilot_class = irelia_puuid.as_ref().and_then(|p| known_pilot_class(&env.db.lock(), p));
    let harvested = irelia.is_some()
        && !mine
        && pilot_class.is_none()
        && env.harvest
        && HARVEST_POSITIONS.contains(&irelia.as_ref().unwrap()["teamPosition"].as_str().unwrap_or(""));
    let me = env.me_puuid.clone();
    let source: Option<String> = match &irelia_puuid {
        None => None,
        Some(p) if Some(p) == me.as_ref() => Some("me".into()),
        Some(_) => pilot_class.clone().or_else(|| if mine { Some(ctx.source.clone()) } else if harvested { Some("harvested".into()) } else { None }),
    };
    let mut game: Option<GameCapture> = None;
    if let (Some(ir), Some(src)) = (&irelia, &source) {
        if keep && !quality.remake {
            let Some(timeline) = env.riot.timeline(platform, id, signal).await? else { return Ok(Fetched::Missing) };
            let pilot = env.db.lock().player(irelia_puuid.as_deref().unwrap_or(""));
            game = Some(capture_game(&m, &timeline, ir, src, pilot.as_ref()));
            row.has_irelia = true;
        }
    }
    let candidate = if harvested && irelia_puuid != me { irelia_puuid.clone().map(|p| (p, platform.to_string())) } else { None };
    Ok(Fetched::Fetched(Box::new(FetchedMatch {
        row,
        quality,
        participants: participant_rows(&m),
        stale: !keep,
        game,
        pilot_played_irelia: mine,
        candidate,
        pilot_puuid: ctx.pilot_puuid.clone(),
        source: ctx.source.clone(),
    })))
}

/// A harvested game whose match is already stored: one timeline call. None when there is nothing to store.
pub async fn fetch_harvested<A: RiotApi>(env: &IngestEnv<'_, A>, platform: &str, id: &str, puuid: &str, signal: &Signal) -> Result<Option<GameCapture>> {
    if env.db.lock().game_source(id).is_some() {
        return Ok(None);
    }
    let m = env.riot.get_match(platform, id, signal).await?;
    let Some(m) = m else { return Ok(None) };
    let ps = participants(&m);
    let Some(irelia) = ps.iter().find(|p| p["championId"].as_i64() == Some(env.champion_id) && p["puuid"].as_str() == Some(puuid)).cloned() else {
        return Ok(None);
    };
    if match_quality(&m).remake {
        return Ok(None);
    }
    if is_stale(&to_patch(m["info"]["gameVersion"].as_str().unwrap_or("")), &(env.region)(platform).patch) {
        return Ok(None);
    }
    let Some(timeline) = env.riot.timeline(platform, id, signal).await? else { return Ok(None) };
    let source = if Some(puuid.to_string()) == env.me_puuid {
        "me".to_string()
    } else {
        known_pilot_class(&env.db.lock(), puuid).unwrap_or_else(|| "harvested".into())
    };
    let pilot = env.db.lock().player(puuid);
    Ok(Some(capture_game(&m, &timeline, &irelia, &source, pilot.as_ref())))
}
