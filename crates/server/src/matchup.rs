//! Port of `server/matchup.ts`: the matchup the app shows (lane, lane opponent, the two teams), resolved once for the
//! dashboard and the overlay alike from the League client, the Live Client and your own picks.

use irelia::types::to_lane;
use serde_json::{json, Value};

/// What the live game says, in champion ids.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveView {
    pub lane: Option<&'static str>,
    pub enemies: Vec<i64>,
    pub allies: Vec<i64>,
    pub position_opponent: Option<i64>,
}

#[derive(Clone, Debug)]
struct Choice<T> {
    value: T,
    phase: &'static str,
}

/// The likely opponent for each lane (lanes asked → opponent).
pub type GuessOpponents = Box<dyn Fn(&[i64], &[&'static str]) -> Vec<(&'static str, Option<i64>)> + Send + Sync>;

pub struct MatchupSession {
    lane: Option<Choice<&'static str>>,
    opponent: Option<Choice<Option<i64>>>,
    regions: Option<Vec<String>>,
    phase: &'static str,
    detected: Option<&'static str>,
    saved: Option<&'static str>,
    guessed: Option<(String, Option<i64>)>,
    saved_lane: Box<dyn Fn() -> Option<&'static str> + Send + Sync>,
    save_lane: Box<dyn Fn(&'static str) + Send + Sync>,
    guess: GuessOpponents,
}

/// Matchup as JSON (shared/types.ts Matchup).
#[derive(Clone, Debug, PartialEq)]
pub struct Matchup {
    pub phase: &'static str,
    pub lane: &'static str,
    pub lane_from: &'static str,
    pub detected_lane: Option<&'static str>,
    pub opponent_id: Option<i64>,
    pub opponent_from: Option<&'static str>,
    pub enemies: Vec<i64>,
    pub allies: Vec<i64>,
    pub regions: Option<Vec<String>>,
    pub key: String,
}

impl Matchup {
    pub fn to_json(&self) -> Value {
        json!({
            "phase": self.phase, "lane": self.lane, "laneFrom": self.lane_from, "detectedLane": self.detected_lane,
            "opponentId": self.opponent_id, "opponentFrom": self.opponent_from, "enemies": self.enemies, "allies": self.allies,
            "regions": self.regions, "key": self.key,
        })
    }
}

fn ids(v: &Value) -> Vec<i64> {
    v.as_array().map(|a| a.iter().map(|p| p["championId"].as_i64().unwrap_or(0)).collect()).unwrap_or_default()
}

impl MatchupSession {
    pub fn new(saved_lane: Box<dyn Fn() -> Option<&'static str> + Send + Sync>, save_lane: Box<dyn Fn(&'static str) + Send + Sync>, guess: GuessOpponents) -> MatchupSession {
        MatchupSession { lane: None, opponent: None, regions: None, phase: "idle", detected: None, saved: None, guessed: None, saved_lane, save_lane, guess }
    }

    /// `lcu`: LcuState as JSON (its champSelect read).
    pub fn resolve(&mut self, lcu: Option<&Value>, live: Option<&LiveView>) -> Matchup {
        let draft = lcu.and_then(|l| l.get("champSelect")).filter(|d| !d.is_null());
        let phase = if live.is_some() { "game" } else if draft.is_some() { "draft" } else { "idle" };
        if phase != self.phase {
            fn keep<T: Clone>(p: &Option<Choice<T>>, phase: &str) -> Option<Choice<T>> {
                p.as_ref().filter(|p| p.phase != "idle" && phase != "idle").cloned()
            }
            self.lane = keep(&self.lane, phase);
            self.opponent = keep(&self.opponent, phase);
            self.phase = phase;
        }
        let draft_lane = to_lane(draft.and_then(|d| d["myPosition"].as_str()));
        self.detected = live.and_then(|l| l.lane).or(draft_lane);
        if self.saved.is_none() {
            self.saved = Some((self.saved_lane)().unwrap_or("TOP"));
        }
        let lane = self.lane.as_ref().map(|l| l.value).or(self.detected).or(self.saved).unwrap_or("TOP");
        let lane_from = if self.lane.is_some() {
            "manual"
        } else if live.and_then(|l| l.lane).is_some() {
            "game"
        } else if draft_lane.is_some() {
            "draft"
        } else {
            "saved"
        };
        let enemies: Vec<i64> = match live {
            Some(l) => l.enemies.clone(),
            None => draft.map(|d| ids(&d["enemies"])).unwrap_or_default().into_iter().filter(|id| *id != 0).collect(),
        };
        let allies: Vec<i64> = match live {
            Some(l) => l.allies.clone(),
            None => {
                let mine = draft.and_then(|d| d["myChampionId"].as_i64());
                draft.map(|d| ids(&d["allies"])).unwrap_or_default().into_iter().filter(|id| *id != 0 && Some(*id) != mine).collect()
            }
        };
        let draft_pick: Option<i64> = draft.and_then(|d| {
            let s = &d["suggestedOpponents"];
            if s.is_object() {
                s[lane].as_i64()
            } else {
                d["suggestedOpponentId"].as_i64()
            }
        });
        let mut opponent_id = None;
        let mut opponent_from = None;
        if let (Some(o), Some(l)) = (&self.opponent, live) {
            if o.phase == "draft" && o.value.map(|v| !l.enemies.contains(&v)).unwrap_or(false) {
                self.opponent = None;
            }
        }
        if let Some(o) = &self.opponent {
            opponent_id = o.value;
            opponent_from = Some("manual");
        } else if let Some(l) = live {
            if l.position_opponent.is_some() {
                opponent_id = l.position_opponent;
                opponent_from = Some("game");
            } else if draft_pick.map(|d| l.enemies.contains(&d)).unwrap_or(false) {
                opponent_id = draft_pick;
                opponent_from = Some("draft");
            } else {
                opponent_id = self.guess_for(&l.enemies, lane);
                opponent_from = opponent_id.map(|_| "guess");
            }
        } else if draft_pick.is_some() {
            opponent_id = draft_pick;
            opponent_from = Some("draft");
        }
        let regions = self.regions.clone();
        let join = |v: &[i64]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let key = [
            lane.to_string(),
            opponent_id.map(|o| o.to_string()).unwrap_or_default(),
            join(&enemies),
            join(&allies),
            regions.as_ref().map(|r| r.join(",")).unwrap_or_default(),
        ]
        .join("|");
        Matchup { phase, lane, lane_from, detected_lane: self.detected, opponent_id, opponent_from, enemies, allies, regions, key }
    }

    fn guess_for(&mut self, enemies: &[i64], lane: &'static str) -> Option<i64> {
        if enemies.is_empty() {
            return None;
        }
        let key = format!("{}|{}", lane, enemies.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","));
        if self.guessed.as_ref().map(|g| g.0 != key).unwrap_or(true) {
            let id = (self.guess)(enemies, &[lane]).into_iter().find(|(l, _)| *l == lane).and_then(|(_, id)| id);
            self.guessed = Some((key, id));
        }
        self.guessed.as_ref().unwrap().1
    }

    pub fn set_lane(&mut self, lane: &'static str) {
        self.saved = Some(lane);
        (self.save_lane)(lane);
        self.lane = match self.detected {
            Some(d) if lane != d => Some(Choice { value: lane, phase: self.phase }),
            _ => None,
        };
    }

    /// You picked an opponent (Some(None): all matchups); None goes back to following the draft and the game.
    pub fn set_opponent(&mut self, id: Option<Option<i64>>) {
        self.opponent = id.map(|value| Choice { value, phase: self.phase });
    }

    pub fn set_regions(&mut self, regions: Option<Vec<String>>) {
        self.regions = regions.filter(|r| !r.is_empty());
    }
}

#[cfg(test)]
mod tests {
    //! Port of server/matchup.test.ts.
    use super::*;
    use std::sync::{Arc, Mutex};

    const DARIUS: i64 = 122;
    const LEESIN: i64 = 64;
    const SYNDRA: i64 = 134;
    const JINX: i64 = 222;
    const THRESH: i64 = 412;
    const AHRI: i64 = 103;
    const IRELIA: i64 = 39;

    fn session(saved: Option<&'static str>, guess: Vec<(&'static str, Option<i64>)>) -> (MatchupSession, Arc<Mutex<Vec<&'static str>>>) {
        let saves = Arc::new(Mutex::new(vec![]));
        let s2 = saves.clone();
        let s = MatchupSession::new(
            Box::new(move || saved),
            Box::new(move |l| s2.lock().unwrap().push(l)),
            Box::new(move |_, lanes| lanes.iter().map(|l| (*l, guess.iter().find(|g| g.0 == *l).and_then(|g| g.1))).collect()),
        );
        (s, saves)
    }

    fn idle() -> Value {
        json!({ "status": "connected", "phase": "Lobby", "champSelect": null })
    }
    fn draft(position: &str, enemies: &[i64], top: Option<i64>, middle: Option<i64>) -> Value {
        json!({ "status": "connected", "phase": "ChampSelect", "champSelect": {
            "myChampionId": IRELIA, "myPosition": position, "isIrelia": true,
            "allies": [{ "cellId": 0, "championId": IRELIA, "position": position, "locked": true }, { "cellId": 1, "championId": AHRI, "position": "", "locked": true }],
            "enemies": enemies.iter().enumerate().map(|(i, id)| json!({ "cellId": 5 + i, "championId": id, "position": "", "locked": true })).collect::<Vec<_>>(),
            "suggestedOpponentId": top, "suggestedOpponents": { "TOP": top, "MIDDLE": middle },
        } })
    }
    fn game(lane: Option<&'static str>, position_opponent: Option<i64>, enemies: Option<Vec<i64>>) -> LiveView {
        LiveView { lane, position_opponent, enemies: enemies.unwrap_or(vec![DARIUS, LEESIN, SYNDRA, JINX, THRESH]), allies: vec![AHRI] }
    }

    #[test]
    fn idle_the_lane_you_picked_last() {
        let (mut s, saves) = session(Some("MIDDLE"), vec![]);
        let m = s.resolve(Some(&idle()), None);
        assert_eq!((m.phase, m.lane, m.lane_from, m.opponent_id), ("idle", "MIDDLE", "saved", None));
        s.set_lane("TOP");
        let m = s.resolve(Some(&idle()), None);
        assert_eq!((m.lane, m.lane_from), ("TOP", "saved"));
        assert_eq!(*saves.lock().unwrap(), vec!["TOP"]);
        s.set_opponent(Some(Some(DARIUS)));
        let m = s.resolve(Some(&idle()), None);
        assert_eq!((m.opponent_id, m.opponent_from), (Some(DARIUS), Some("manual")));
        assert_eq!(session(None, vec![]).0.resolve(Some(&idle()), None).lane, "TOP");
    }

    #[test]
    fn champ_select_assigned_lane_and_picks() {
        let (mut s, _) = session(Some("TOP"), vec![]);
        s.set_opponent(Some(Some(SYNDRA)));
        let d = draft("MIDDLE", &[DARIUS, SYNDRA], Some(DARIUS), Some(SYNDRA));
        let m = s.resolve(Some(&d), None);
        assert_eq!((m.phase, m.lane, m.lane_from, m.opponent_id, m.opponent_from), ("draft", "MIDDLE", "draft", Some(SYNDRA), Some("draft")));
        assert_eq!((m.enemies.clone(), m.allies.clone()), (vec![DARIUS, SYNDRA], vec![AHRI]));
        s.set_lane("TOP");
        let m = s.resolve(Some(&d), None);
        assert_eq!((m.lane, m.lane_from, m.detected_lane, m.opponent_id), ("TOP", "manual", Some("MIDDLE"), Some(DARIUS)));
        s.set_lane("MIDDLE");
        assert_eq!(s.resolve(Some(&d), None).lane_from, "draft");
        s.set_opponent(Some(Some(DARIUS)));
        let m = s.resolve(Some(&d), None);
        assert_eq!((m.opponent_id, m.opponent_from), (Some(DARIUS), Some("manual")));
        s.set_opponent(None);
        assert_eq!(s.resolve(Some(&d), None).opponent_from, Some("draft"));
        s.set_opponent(Some(Some(DARIUS)));
        let m = s.resolve(Some(&idle()), None);
        assert_eq!((m.phase, m.lane, m.opponent_id), ("idle", "MIDDLE", None));
    }

    #[test]
    fn in_game_position_then_draft_then_guess() {
        let lcu = draft("TOP", &[DARIUS, LEESIN, SYNDRA, JINX, THRESH], Some(DARIUS), Some(SYNDRA));
        let m = session(None, vec![]).0.resolve(Some(&lcu), Some(&game(Some("TOP"), Some(DARIUS), None)));
        assert_eq!((m.phase, m.lane, m.lane_from, m.opponent_id, m.opponent_from), ("game", "TOP", "game", Some(DARIUS), Some("game")));
        let m = session(None, vec![]).0.resolve(Some(&lcu), Some(&game(None, None, None)));
        assert_eq!((m.lane, m.lane_from, m.opponent_id, m.opponent_from), ("TOP", "draft", Some(DARIUS), Some("draft")));
        let (mut s, _) = session(Some("MIDDLE"), vec![("MIDDLE", Some(SYNDRA))]);
        let m = s.resolve(Some(&idle()), Some(&game(None, None, Some(vec![AHRI, JINX, SYNDRA]))));
        assert_eq!((m.lane, m.lane_from, m.opponent_id, m.opponent_from), ("MIDDLE", "saved", Some(SYNDRA), Some("guess")));
        assert_eq!(session(Some("TOP"), vec![]).0.resolve(Some(&idle()), Some(&game(None, None, None))).opponent_from, None);
    }

    #[test]
    fn champ_select_pick_carries_into_its_game() {
        let (mut s, _) = session(None, vec![]);
        let lcu = draft("TOP", &[DARIUS, LEESIN, SYNDRA, JINX, THRESH], Some(DARIUS), None);
        s.resolve(Some(&lcu), None);
        s.set_opponent(Some(Some(LEESIN)));
        let m = s.resolve(Some(&lcu), Some(&game(Some("TOP"), Some(DARIUS), None)));
        assert_eq!((m.opponent_id, m.opponent_from), (Some(LEESIN), Some("manual")));
        let (mut other, _) = session(None, vec![]);
        other.resolve(Some(&lcu), None);
        other.set_opponent(Some(Some(AHRI)));
        let m = other.resolve(Some(&lcu), Some(&game(Some("TOP"), Some(DARIUS), None)));
        assert_eq!((m.opponent_id, m.opponent_from), (Some(DARIUS), Some("game")));
        s.set_opponent(Some(Some(SYNDRA)));
        let m = s.resolve(Some(&lcu), Some(&game(Some("TOP"), Some(DARIUS), None)));
        assert_eq!((m.opponent_id, m.opponent_from), (Some(SYNDRA), Some("manual")));
        let m = s.resolve(Some(&idle()), None);
        assert_eq!((m.phase, m.opponent_id), ("idle", None));
    }

    #[test]
    fn key_changes_with_what_the_build_depends_on() {
        let (mut s, _) = session(None, vec![]);
        let a = s.resolve(Some(&draft("TOP", &[DARIUS], Some(DARIUS), None)), None);
        let b = s.resolve(Some(&draft("TOP", &[DARIUS, SYNDRA], Some(DARIUS), None)), None);
        assert_ne!(a.key, b.key);
        s.set_regions(Some(vec!["KR".into()]));
        let c = s.resolve(Some(&draft("TOP", &[DARIUS, SYNDRA], Some(DARIUS), None)), None);
        assert_eq!(c.regions, Some(vec!["KR".to_string()]));
        assert_ne!(b.key, c.key);
        s.set_regions(Some(vec![]));
        assert_eq!(s.resolve(Some(&draft("TOP", &[DARIUS, SYNDRA], Some(DARIUS), None)), None).key, b.key);
    }

    #[test]
    fn in_game_pick_survives_a_reconnect() {
        let (mut s, _) = session(None, vec![]);
        let lcu = draft("TOP", &[DARIUS, LEESIN, SYNDRA, JINX, THRESH], Some(DARIUS), None);
        s.resolve(Some(&lcu), Some(&game(Some("TOP"), Some(DARIUS), None)));
        s.set_opponent(Some(Some(SYNDRA)));
        s.set_lane("MIDDLE");
        assert_eq!(s.resolve(Some(&lcu), None).phase, "draft");
        let m = s.resolve(Some(&lcu), Some(&game(Some("TOP"), Some(DARIUS), None)));
        assert_eq!((m.lane, m.lane_from, m.opponent_id, m.opponent_from), ("MIDDLE", "manual", Some(SYNDRA), Some("manual")));
        let after = s.resolve(Some(&idle()), None);
        assert_eq!((after.opponent_id, after.lane_from), (None, "saved"));
    }
}
