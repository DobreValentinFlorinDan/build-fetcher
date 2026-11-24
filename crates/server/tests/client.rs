//! The League client's events through the app, as a real client delivers them (the connector's own handler and its
//! callback into the app). 2.1.0 and earlier 1.x/2.x froze at the first champ-select event: the callback kept the
//! app reference locked while the app paused the fetcher, whose status report then waited for that same lock. The
//! simulator routes never took that path, so nothing showed it.
mod common;

use serde_json::json;
use std::time::Duration;

fn session(enemy: i64) -> serde_json::Value {
    json!({
        "localPlayerCellId": 0,
        "actions": [[{ "type": "pick", "completed": true, "actorCellId": 0 }, { "type": "pick", "completed": true, "actorCellId": 5 }]],
        "myTeam": [{ "cellId": 0, "championId": 39, "assignedPosition": "top" }, { "cellId": 1, "championId": 64, "assignedPosition": "jungle" }],
        "theirTeam": [{ "cellId": 5, "championId": enemy, "assignedPosition": "" }],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn champ_select_and_the_game_are_followed_through_the_clients_own_events() {
    let app = common::start(|_, _| {}).await;
    // Each event must be handled promptly. On its own thread: a handler that never returns fails the test here
    // instead of hanging it.
    let event = |uri: &'static str, data: serde_json::Value| {
        let a = app.app.clone();
        async move {
            let (tx, rx) = std::sync::mpsc::channel();
            let runtime = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                let _in = runtime.enter(); // the client's events arrive on the runtime
                a.lcu.inject_event(uri, "Update", &data);
                let _ = tx.send(());
            });
            let handled = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(20)).is_ok()).await.unwrap();
            if !handled {
                eprintln!("the client event {uri} was never handled (deadlock)");
                std::process::exit(1); // the stuck thread would keep the test binary alive
            }
        }
    };
    const PHASE: &str = "/lol-gameflow/v1/gameflow-phase";
    const SESSION: &str = "/lol-champ-select/v1/session";
    // No League client runs here, so every 3 s the connector's own search puts the state back to "no client". A
    // step that such a reset fell into is sent again from the start (the events are the client's whole story so far).
    let mut story: Vec<(&'static str, serde_json::Value)> = vec![];
    let mut after = |uri: &'static str, data: serde_json::Value, phase: &'static str| {
        story.push((uri, data));
        let story = story.clone();
        let (event, app) = (&event, &app);
        async move {
            for _ in 0..5 {
                for (uri, data) in &story {
                    event(uri, data.clone()).await;
                }
                let state = app.get("/api/state").await;
                if state["lcu"]["phase"] == phase {
                    return state;
                }
            }
            panic!("the client's phase never became {phase}");
        }
    };
    after(PHASE, json!("ReadyCheck"), "ReadyCheck").await;
    after(PHASE, json!("ChampSelect"), "ChampSelect").await;
    let state = after(SESSION, session(122), "ChampSelect").await;
    assert_eq!(state["lcu"]["champSelect"]["isIrelia"], true);
    assert_eq!(state["matchup"]["phase"], "draft");
    assert_eq!(state["matchup"]["lane"], "TOP");
    assert_eq!(state["matchup"]["enemies"], json!([122]));
    // A second pick, the game starting, and its end: every change still arrives.
    let state = after(SESSION, session(86), "ChampSelect").await;
    assert_eq!(state["matchup"]["enemies"], json!([86]));
    after(PHASE, json!("InProgress"), "InProgress").await;
    let state = after(PHASE, json!("EndOfGame"), "EndOfGame").await;
    assert_eq!(state["matchup"]["phase"], "idle");
}
