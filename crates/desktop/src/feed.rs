//! `watchServer` of `desktop/main.ts`: the shell follows the server's `/ws` feed. 'live' says whether a game is running
//! (the overlay), 'scraper' whether fetching is on and 'jobs' whether an evaluation you started runs (keeping the
//! computer awake), 'matchup' whether champ select or a game is on (the dashboard's priority). Reconnects forever;
//! while disconnected, fetching and evaluating count as off.

use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    Game(bool),
    Fetching(bool),
    Evaluating(bool),
    Playing(bool),
}

/// What one `/ws` frame says, if it is one the shell follows.
pub fn parse_feed(text: &str) -> Option<Feed> {
    let ev: Value = serde_json::from_str(text).ok()?;
    let truthy = |v: Option<&Value>| v.is_some_and(irelia::js::truthy);
    match ev.get("type")?.as_str()? {
        "live" => Some(Feed::Game(truthy(ev.pointer("/state/active")))),
        "scraper" => Some(Feed::Fetching(truthy(ev.pointer("/status/running")))),
        "jobs" => {
            let running = ev.pointer("/status/running").and_then(Value::as_array);
            let user_eval = running.is_some_and(|rs| {
                rs.iter().any(|r| r.get("job").and_then(Value::as_str) == Some("evaluate") && !truthy(r.get("auto")))
            });
            Some(Feed::Evaluating(user_eval))
        }
        "matchup" => {
            let phase = ev.pointer("/matchup/phase");
            let on = truthy(phase) && phase.and_then(Value::as_str) != Some("idle");
            Some(Feed::Playing(on))
        }
        _ => None,
    }
}

/// Follows `{origin}/ws` until the returned task is aborted.
pub fn watch_server(origin: &str, on: impl Fn(Feed) + Send + Sync + 'static) -> tokio::task::JoinHandle<()> {
    let url = format!("{}/ws", origin.replacen("http", "ws", 1));
    tokio::spawn(async move {
        loop {
            if let Ok((mut ws, _)) = tokio_tungstenite::connect_async(url.as_str()).await {
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        if let Some(f) = parse_feed(&t) {
                            on(f);
                        }
                    }
                }
            }
            on(Feed::Fetching(false));
            on(Feed::Evaluating(false));
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_the_shell_follows() {
        assert_eq!(parse_feed(r#"{"type":"live","state":{"active":true}}"#), Some(Feed::Game(true)));
        assert_eq!(parse_feed(r#"{"type":"live","state":null}"#), Some(Feed::Game(false)));
        assert_eq!(parse_feed(r#"{"type":"scraper","status":{"running":true}}"#), Some(Feed::Fetching(true)));
        assert_eq!(
            parse_feed(r#"{"type":"jobs","status":{"running":[{"job":"evaluate","auto":"patch"}]}}"#),
            Some(Feed::Evaluating(false)),
            "one the app started by itself"
        );
        assert_eq!(parse_feed(r#"{"type":"jobs","status":{"running":[{"job":"audit"},{"job":"evaluate"}]}}"#), Some(Feed::Evaluating(true)));
        assert_eq!(parse_feed(r#"{"type":"jobs","status":{"running":false}}"#), Some(Feed::Evaluating(false)));
        assert_eq!(parse_feed(r#"{"type":"matchup","matchup":{"phase":"champSelect"}}"#), Some(Feed::Playing(true)));
        assert_eq!(parse_feed(r#"{"type":"matchup","matchup":{"phase":"idle"}}"#), Some(Feed::Playing(false)));
        assert_eq!(parse_feed(r#"{"type":"matchup","matchup":null}"#), Some(Feed::Playing(false)));
        assert_eq!(parse_feed(r#"{"type":"model"}"#), None);
        assert_eq!(parse_feed("not json"), None);
    }
}
