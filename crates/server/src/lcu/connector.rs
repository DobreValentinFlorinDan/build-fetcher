//! Port of `server/lcu/connector.ts`: finds the running League client (lockfile, else the LeagueClientUx command
//! line), opens its WAMP websocket on 127.0.0.1 and follows champ select and the game flow phase.

use super::https::{loopback_json, loopback_request, loopback_tls};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use irelia::types::to_lane;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const LANES: [&str; 2] = ["TOP", "MIDDLE"];
const OTHER_POSITIONS: [&str; 3] = ["JUNGLE", "BOTTOM", "UTILITY"];
const SUBSCRIPTIONS: [&str; 2] = ["OnJsonApiEvent_lol-champ-select_v1_session", "OnJsonApiEvent_lol-gameflow_v1_gameflow-phase"];
const KEEPS_DRAFT: [&str; 4] = ["ChampSelect", "GameStart", "InProgress", "Reconnect"];
const AFTER_GAME: [&str; 3] = ["PreEndOfGame", "WaitingForStats", "EndOfGame"];

pub type GuessOpponents = Arc<dyn Fn(&[i64], &[&'static str]) -> Vec<(&'static str, Option<i64>)> + Send + Sync>;

/// Draft state from an LCU champ-select session (LcuState.champSelect as JSON).
pub fn parse_champ_select(s: &Value, irelia_id: i64, guess: &dyn Fn(&[i64], &[&'static str]) -> Vec<(&'static str, Option<i64>)>) -> Value {
    let locked: HashSet<i64> = s["actions"]
        .as_array()
        .map(|groups| {
            groups
                .iter()
                .filter_map(|g| g.as_array())
                .flatten()
                .filter(|a| a["type"] == "pick" && a["completed"] == true)
                .filter_map(|a| a["actorCellId"].as_i64())
                .collect()
        })
        .unwrap_or_default();
    let champ = |p: &Value| {
        let c = p["championId"].as_i64().unwrap_or(0);
        if c != 0 {
            c
        } else {
            p["championPickIntent"].as_i64().unwrap_or(0)
        }
    };
    let position = |p: &Value| p["assignedPosition"].as_str().unwrap_or("").to_uppercase();
    let pick = |p: &Value| json!({ "cellId": p["cellId"], "championId": champ(p), "position": position(p), "locked": locked.contains(&p["cellId"].as_i64().unwrap_or(-1)) });
    let my_team: Vec<Value> = s["myTeam"].as_array().cloned().unwrap_or_default();
    let their: Vec<Value> = s["theirTeam"].as_array().cloned().unwrap_or_default();
    let me = my_team.iter().find(|p| p["cellId"] == s["localPlayerCellId"]);
    let my_champion = me.map(champ).unwrap_or(0);
    let my_position = me.map(position).unwrap_or_default();
    let enemies: Vec<Value> = their.iter().map(pick).collect();
    let shown = |lane: &str| {
        enemies
            .iter()
            .find(|e| e["championId"].as_i64().unwrap_or(0) != 0 && to_lane(e["position"].as_str()) == Some(lane))
            .and_then(|e| e["championId"].as_i64())
    };
    let role_shown = |e: &Value| to_lane(e["position"].as_str()).is_some() || OTHER_POSITIONS.contains(&e["position"].as_str().unwrap_or(""));
    let hidden: Vec<i64> = enemies.iter().filter(|e| e["championId"].as_i64().unwrap_or(0) != 0 && !role_shown(e)).filter_map(|e| e["championId"].as_i64()).collect();
    let open: Vec<&'static str> = LANES.iter().copied().filter(|l| shown(l).is_none()).collect();
    let guessed = if !open.is_empty() && !hidden.is_empty() { guess(&hidden, &open) } else { vec![] };
    let mut suggested = Map::new();
    for lane in LANES {
        let v = shown(lane).or_else(|| guessed.iter().find(|g| g.0 == lane).and_then(|g| g.1));
        suggested.insert(lane.into(), json!(v));
    }
    let my_lane = to_lane(Some(&my_position)).unwrap_or("TOP");
    json!({
        "myChampionId": my_champion,
        "myPosition": my_position,
        "isIrelia": my_champion == irelia_id,
        "allies": my_team.iter().map(pick).collect::<Vec<_>>(),
        "enemies": enemies,
        "suggestedOpponentId": suggested[my_lane].clone(),
        "suggestedOpponents": Value::Object(suggested),
    })
}

/// The signed-in account, from the League client.
#[derive(Clone, Debug, PartialEq)]
pub struct LcuAccount {
    pub puuid: String,
    pub game_name: Option<String>,
    pub tag_line: Option<String>,
    pub platform: Option<String>,
}

pub enum LcuEvent {
    State(Value),
    Account(LcuAccount),
    GameEnded,
}

#[derive(Clone)]
struct Creds {
    port: u16,
    password: String,
}

pub struct LcuConnector {
    pub state: Mutex<Value>,
    pub account: Mutex<Option<LcuAccount>>,
    creds: Mutex<Option<Creds>>,
    league_paths: Vec<PathBuf>,
    irelia_id: i64,
    guess: GuessOpponents,
    emit: Arc<dyn Fn(LcuEvent) + Send + Sync>,
    /// The 5 s phase poll runs once for the connector's lifetime, across client restarts.
    poll_started: AtomicBool,
}

impl LcuConnector {
    pub fn new(league_paths: Vec<PathBuf>, irelia_id: i64, guess: GuessOpponents, emit: Arc<dyn Fn(LcuEvent) + Send + Sync>) -> Arc<LcuConnector> {
        Arc::new(LcuConnector {
            state: Mutex::new(json!({ "status": "searching", "phase": "None", "champSelect": null })),
            account: Mutex::new(None),
            creds: Mutex::new(None),
            league_paths,
            irelia_id,
            guess,
            emit,
            poll_started: AtomicBool::new(false),
        })
    }

    pub fn state(&self) -> Value {
        self.state.lock().clone()
    }

    fn set(&self, patch: Value) {
        let s = {
            let mut st = self.state.lock();
            let mut changed = false;
            if let (Some(o), Some(p)) = (st.as_object_mut(), patch.as_object()) {
                for (k, v) in p {
                    if o.get(k) != Some(v) {
                        o.insert(k.clone(), v.clone());
                        changed = true;
                    }
                }
            }
            if !changed {
                return;
            }
            st.clone()
        };
        (self.emit)(LcuEvent::State(s));
    }

    pub fn start(self: &Arc<Self>) {
        self.start_poll();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match me.discover().await {
                    None => {
                        me.set(json!({ "status": "searching", "detail": "League client not running", "phase": "None", "champSelect": null }));
                    }
                    Some(c) => {
                        *me.creds.lock() = Some(c.clone());
                        me.set(json!({ "status": "connecting", "detail": format!("127.0.0.1:{}", c.port) }));
                        me.clone().connect(c).await;
                        *me.creds.lock() = None;
                        me.set(json!({ "status": "disconnected", "detail": "Client closed", "phase": "None", "champSelect": null }));
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }

    /// A phase/session poll beside the websocket. The client sometimes stops pushing WAMP events on a connection that
    /// stays open (after sleep/resume, or a long idle): the state then froze at the last event, and the overlay stayed
    /// hidden through a game. Five seconds is cheap on loopback and only changes state when something differs.
    fn start_poll(self: &Arc<Self>) {
        if self.poll_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if me.creds.lock().is_some() {
                    me.poll().await;
                }
            }
        });
    }

    async fn poll(&self) {
        let Ok(phase) = self.request("/lol-gameflow/v1/gameflow-phase").await else { return };
        self.handle_event("/lol-gameflow/v1/gameflow-phase", "Update", &phase);
        if self.state.lock()["phase"] == "ChampSelect" {
            match self.request("/lol-champ-select/v1/session").await {
                Ok(session) => self.handle_event("/lol-champ-select/v1/session", "Update", &session),
                Err(_) => self.handle_event("/lol-champ-select/v1/session", "Delete", &Value::Null),
            }
        }
    }

    async fn discover(&self) -> Option<Creds> {
        for dir in &self.league_paths {
            if let Ok(text) = tokio::fs::read_to_string(dir.join("lockfile")).await {
                let parts: Vec<&str> = text.split(':').collect();
                if parts.len() >= 4 && !parts[2].is_empty() && !parts[3].is_empty() {
                    if let Ok(port) = parts[2].trim().parse() {
                        return Some(Creds { port, password: parts[3].trim().to_string() });
                    }
                }
            }
        }
        if !cfg!(windows) {
            return None;
        }
        let out = tokio::task::spawn_blocking(|| {
            let mut cmd = std::process::Command::new("powershell.exe");
            cmd.args(["-NoProfile", "-Command", "(Get-CimInstance Win32_Process -Filter \"name='LeagueClientUx.exe'\").CommandLine"]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000);
            }
            cmd.output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        let port = regex_lite::Regex::new(r"--app-port=(\d+)").ok()?.captures(&out).and_then(|c| c[1].parse().ok());
        let password = regex_lite::Regex::new(r"--remoting-auth-token=([\w-]+)").ok()?.captures(&out).map(|c| c[1].to_string());
        match (port, password) {
            (Some(port), Some(password)) => Some(Creds { port, password }),
            _ => None,
        }
    }

    fn auth(c: &Creds) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("riot:{}", c.password)))
    }

    pub async fn request(&self, endpoint: &str) -> anyhow::Result<Value> {
        let c = self.creds.lock().clone().ok_or_else(|| anyhow::anyhow!("LCU not connected"))?;
        loopback_json(c.port, endpoint, Some(&Self::auth(&c)), 2500).await
    }

    /// Any method: the Stage C import is the only writer, on your click in champ select.
    pub async fn send(&self, method: &str, endpoint: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let c = self.creds.lock().clone().ok_or_else(|| anyhow::anyhow!("The League client is not connected"))?;
        loopback_request(c.port, method, endpoint, &Self::auth(&c), body, 5000).await
    }

    async fn connect(self: Arc<Self>, c: Creds) {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let Ok(mut req) = format!("wss://127.0.0.1:{}/", c.port).into_client_request() else { return };
        let h = req.headers_mut();
        h.insert("Authorization", Self::auth(&c).parse().unwrap());
        h.insert("Sec-WebSocket-Protocol", "wamp".parse().unwrap());
        let connector = tokio_tungstenite::Connector::Rustls(loopback_tls());
        let Ok(Ok((ws, _))) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(connector))).await else {
            return;
        };
        let (mut tx, mut rx) = ws.split();
        for topic in SUBSCRIPTIONS {
            if tx.send(tokio_tungstenite::tungstenite::Message::Text(json!([5, topic]).to_string())).await.is_err() {
                return;
            }
        }
        self.set(json!({ "status": "connected", "detail": format!("127.0.0.1:{}", c.port) }));
        let boot = self.clone();
        tokio::spawn(async move { boot.bootstrap().await });
        while let Some(Ok(msg)) = rx.next().await {
            let text = match msg {
                tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            if v[0].as_i64() != Some(8) {
                continue;
            }
            let p = &v[2];
            self.handle_event(p["uri"].as_str().unwrap_or(""), p["eventType"].as_str().unwrap_or(""), &p["data"]);
        }
    }

    async fn platform(&self) -> Option<String> {
        let as_text = |v: &Value| v.as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Ok(chat) = self.request("/lol-chat/v1/me").await {
            if let Some(p) = as_text(&chat["platformId"]) {
                return Some(p);
            }
        }
        if let Ok(l) = self.request("/riotclient/region-locale").await {
            if let Some(p) = as_text(&l["region"]) {
                return Some(p);
            }
        }
        self.request("/lol-platform-config/v1/namespaces/LoginDataPacket/platformId").await.ok().and_then(|v| as_text(&v))
    }

    async fn bootstrap(&self) {
        if let Ok(me) = self.request("/lol-summoner/v1/current-summoner").await {
            let name = match me["gameName"].as_str().filter(|g| !g.is_empty()) {
                Some(g) => json!(format!("{}#{}", g, me["tagLine"].as_str().unwrap_or("undefined"))),
                None => me["displayName"].clone(),
            };
            self.set(json!({ "summonerName": name }));
            if let Some(puuid) = me["puuid"].as_str().filter(|p| !p.is_empty()) {
                let account = LcuAccount {
                    puuid: puuid.into(),
                    game_name: me["gameName"].as_str().filter(|s| !s.is_empty()).map(String::from),
                    tag_line: me["tagLine"].as_str().filter(|s| !s.is_empty()).map(String::from),
                    platform: self.platform().await,
                };
                *self.account.lock() = Some(account.clone());
                (self.emit)(LcuEvent::Account(account));
            }
        }
        if let Ok(phase) = self.request("/lol-gameflow/v1/gameflow-phase").await {
            self.handle_event("/lol-gameflow/v1/gameflow-phase", "Update", &phase);
        }
        if let Ok(s) = self.request("/lol-champ-select/v1/session").await {
            self.handle_event("/lol-champ-select/v1/session", "Update", &s);
        }
    }

    /// A client event as its websocket delivers one (the dev route `/api/dev/client-event` and the tests: the same path
    /// as a real client's, callbacks included).
    pub fn inject_event(&self, uri: &str, event_type: &str, data: &Value) {
        self.handle_event(uri, event_type, data);
    }

    fn handle_event(&self, uri: &str, event_type: &str, data: &Value) {
        if uri == "/lol-gameflow/v1/gameflow-phase" {
            let phase = match data {
                Value::Null => "None".to_string(),
                Value::String(s) => s.clone(),
                other => irelia::js::stringify(other),
            };
            let before = self.state.lock()["phase"].as_str().unwrap_or("").to_string();
            if AFTER_GAME.contains(&phase.as_str()) && !AFTER_GAME.contains(&before.as_str()) {
                (self.emit)(LcuEvent::GameEnded);
            }
            if KEEPS_DRAFT.contains(&phase.as_str()) {
                self.set(json!({ "phase": phase }));
            } else {
                self.set(json!({ "phase": phase, "champSelect": null }));
            }
        } else if uri == "/lol-champ-select/v1/session" {
            if event_type == "Delete" || data.is_null() {
                let (phase, cs) = {
                    let st = self.state.lock();
                    (st["phase"].as_str().unwrap_or("").to_string(), st["champSelect"].clone())
                };
                return self.set(json!({ "champSelect": if KEEPS_DRAFT.contains(&phase.as_str()) { cs } else { Value::Null } }));
            }
            let cs = parse_champ_select(data, self.irelia_id, &*self.guess);
            self.set(json!({ "champSelect": cs }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn set_emits_only_when_the_state_changes() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let c = LcuConnector::new(
            vec![],
            39,
            Arc::new(|_, _| vec![]),
            Arc::new(move |_| {
                h.fetch_add(1, Ordering::SeqCst);
            }),
        );
        c.set(json!({ "phase": "Lobby" }));
        c.set(json!({ "phase": "Lobby" }));
        c.set(json!({ "phase": "ChampSelect" }));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}
