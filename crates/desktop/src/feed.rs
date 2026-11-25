//! The shell follows the server's `/ws` feed (from irelia-companion's `watchServer`): `live` says whether a game is
//! running (the overlay), `status` whether fetching runs (keeping the computer awake), `activity` whether champ select
//! or a game is on (the dashboard's priority). Reconnects forever; while disconnected, fetching counts as off.

use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    Game(bool),
    Fetching(bool),
    Playing(bool),
}

/// What one `/ws` frame says, if it is one the shell follows.
pub fn parse_feed(text: &str) -> Option<Feed> {
    let ev: Value = serde_json::from_str(text).ok()?;
    let truthy = |v: Option<&Value>| v.is_some_and(bf_core::js::truthy);
    match ev.get("type")?.as_str()? {
        "live" => Some(Feed::Game(truthy(ev.pointer("/state/active")))),
        "status" => Some(Feed::Fetching(truthy(ev.pointer("/status/fetch/running")))),
        "activity" => {
            let a = ev.get("activity").and_then(Value::as_str);
            Some(Feed::Playing(matches!(a, Some("champSelect" | "inGame"))))
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
        assert_eq!(parse_feed(r#"{"type":"live","state":{"active":false}}"#), Some(Feed::Game(false)));
        assert_eq!(parse_feed(r#"{"type":"status","status":{"fetch":{"running":true}}}"#), Some(Feed::Fetching(true)));
        assert_eq!(parse_feed(r#"{"type":"status","status":null}"#), Some(Feed::Fetching(false)));
        assert_eq!(parse_feed(r#"{"type":"activity","activity":"champSelect"}"#), Some(Feed::Playing(true)));
        assert_eq!(parse_feed(r#"{"type":"activity","activity":"inGame"}"#), Some(Feed::Playing(true)));
        assert_eq!(parse_feed(r#"{"type":"activity","activity":"busy"}"#), Some(Feed::Playing(false)));
        assert_eq!(parse_feed(r#"{"type":"draft","draft":null}"#), None);
        assert_eq!(parse_feed("not json"), None);
    }
}
