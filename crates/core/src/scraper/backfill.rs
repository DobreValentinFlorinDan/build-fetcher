//! Port of `server/scraper/backfill.ts`: rebuilds the v3 capture of every stored match from the Riot responses on
//! disk (zero API calls). Each batch is read from disk first, then written in one transaction; interrupted, it
//! resumes where it stopped.

use super::pilots::HARVEST_SCORE;
use crate::capture::{capture_timeline, lane_opponents, leavers, match_quality, participant_rows, CAPTURE_VERSION};
use crate::db::{Db, QueuedMatch};
use crate::ddragon::to_patch;
use crate::patches::{is_stale, RegionPatch};
use crate::riot::disk_cache::DiskCache;
use crate::riot::rate_limiter::Signal;
use crate::riot::routing::normalize_platform;
use anyhow::Result;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

const HARVEST_POSITIONS: [&str; 2] = ["TOP", "MIDDLE"];

pub struct BackfillEnv<'a> {
    pub db: &'a Arc<Mutex<Db>>,
    pub cache: &'a Arc<Mutex<DiskCache>>,
    pub champion_id: i64,
    pub harvest: bool,
    pub region: &'a (dyn Fn(&str) -> RegionPatch + Send + Sync),
    pub me_puuid: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackfillProgress {
    pub phase: String,
    pub done: i64,
    pub total: i64,
    pub games: i64,
    pub missing: i64,
    pub harvested: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

impl BackfillProgress {
    pub fn to_json(&self) -> Value {
        json!({
            "phase": self.phase, "done": self.done, "total": self.total, "games": self.games, "missing": self.missing,
            "harvested": self.harvested, "startedAt": self.started_at, "finishedAt": self.finished_at,
        })
    }
}

pub fn run_backfill(env: &BackfillEnv, signal: &Signal, on_progress: &mut dyn FnMut(&BackfillProgress), force: bool, batch: Option<i64>) -> Result<BackfillProgress> {
    let mut p = BackfillProgress { phase: "Preparing".into(), started_at: crate::util::now_ms(), ..Default::default() };
    let step = |p: &mut BackfillProgress, phase: &str, on_progress: &mut dyn FnMut(&BackfillProgress)| {
        p.phase = phase.into();
        on_progress(p);
    };
    {
        let db = env.db.lock();
        if force {
            db.reset_detail()?;
        }
        db.rebuild_game_versions()?;
        if let Some(me) = &env.me_puuid {
            db.tag_own_games(me)?;
        }
    }
    step(&mut p, "Reading cached mastery checks", on_progress);
    let missing_activity = env.db.lock().players_missing_activity()?;
    for player in missing_activity {
        signal.check()?;
        let top = env.cache.lock().get("mastery", &player.platform, &player.puuid, None);
        let first = top.as_ref().and_then(|t| t.get(0)).cloned();
        if let Some(f) = first.filter(|f| f["championId"].as_i64() == Some(env.champion_id)) {
            env.db.lock().set_irelia_activity(&player.puuid, f["championPoints"].as_i64().unwrap_or(0), f["lastPlayTime"].as_i64(), Some(1))?;
        }
    }
    p.total = env.db.lock().count_pending_detail(CAPTURE_VERSION);
    step(&mut p, "Capturing matches from the disk cache", on_progress);
    let size = batch.unwrap_or(50);
    loop {
        signal.check()?;
        let rows = env.db.lock().pending_detail(CAPTURE_VERSION, size)?;
        if rows.is_empty() {
            break;
        }
        struct Loaded {
            r: crate::db::PendingDetail,
            platform: Option<String>,
            m: Option<Value>,
            timeline: Option<Value>,
        }
        let loaded: Vec<Loaded> = rows
            .into_iter()
            .map(|r| {
                let Ok(platform) = normalize_platform(&r.platform) else { return Loaded { r, platform: None, m: None, timeline: None } };
                let m = env.cache.lock().get("match", &platform, &r.match_id, None);
                let timeline = if r.game_puuid.is_some() && m.is_some() { env.cache.lock().get("timeline", &platform, &r.match_id, None) } else { None };
                Loaded { r, platform: Some(platform), m, timeline }
            })
            .collect();
        let n = loaded.len() as i64;
        {
            let db = env.db.lock();
            db.tx(|| {
                for l in &loaded {
                    let res: Result<()> = (|| {
                        let (Some(m), Some(platform)) = (&l.m, &l.platform) else {
                            p.missing += 1;
                            db.mark_detail(&l.r.match_id, CAPTURE_VERSION)?;
                            return Ok(());
                        };
                        let quality = match_quality(m);
                        db.insert_participants(&l.r.match_id, &participant_rows(m))?;
                        db.set_quality(&l.r.match_id, quality.remake, quality.end_result.as_deref(), CAPTURE_VERSION)?;
                        let ps = m["info"]["participants"].as_array().cloned().unwrap_or_default();
                        let irelia = ps.iter().find(|x| x["championId"].as_i64() == Some(env.champion_id));
                        let irelia_puuid = irelia.and_then(|i| i["puuid"].as_str());
                        if let (Some(gp), Some(ir)) = (&l.r.game_puuid, irelia) {
                            if irelia_puuid == Some(gp.as_str()) {
                                let Some(tl) = &l.timeline else {
                                    p.missing += 1;
                                    return Ok(());
                                };
                                let cap = capture_timeline(tl);
                                let opp = lane_opponents(m, Some(&cap), ir);
                                db.insert_timeline(&l.r.match_id, cap.interval as i64, &cap.to_json_string())?;
                                db.set_leaver(&l.r.match_id, !leavers(&cap).is_empty())?;
                                db.set_opponents(&l.r.match_id, ir["participantId"].as_i64().unwrap_or(0), opp.position, opp.timeline, opp.effective)?;
                                if let Some(pilot) = db.player(gp) {
                                    db.fill_pilot_meta(&l.r.match_id, &pilot)?;
                                }
                                p.games += 1;
                                return Ok(());
                            }
                        }
                        if let Some(ir) = irelia {
                            if l.r.game_puuid.is_none()
                                && env.harvest
                                && HARVEST_POSITIONS.contains(&ir["teamPosition"].as_str().unwrap_or(""))
                                && irelia_puuid.map(String::from) != env.me_puuid
                            {
                                let puuid = irelia_puuid.unwrap_or("");
                                db.add_candidate(puuid, platform, "harvest")?;
                                if !quality.remake && !is_stale(&to_patch(m["info"]["gameVersion"].as_str().unwrap_or("")), &(env.region)(platform).patch) {
                                    db.enqueue(&QueuedMatch {
                                        match_id: l.r.match_id.clone(),
                                        platform: platform.clone(),
                                        kind: "timeline".into(),
                                        puuid: puuid.into(),
                                        source: "harvested".into(),
                                        score: HARVEST_SCORE,
                                    })?;
                                    p.harvested += 1;
                                }
                            }
                        }
                        Ok(())
                    })();
                    if res.is_err() {
                        p.missing += 1;
                        db.mark_detail(&l.r.match_id, CAPTURE_VERSION)?;
                    }
                }
                Ok(())
            })?;
        }
        p.done += n;
        on_progress(&p);
    }
    p.finished_at = Some(crate::util::now_ms());
    step(&mut p, "Done", on_progress);
    Ok(p)
}
