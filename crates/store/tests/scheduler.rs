//! The fetcher: priority order, timelines, pause and resume, and the shared-key budget end to end.

use bf_core::build::{Activity, Platform, RankBand, Routing};
use bf_core::config::parse_limits;
use bf_core::riot::client::{LeagueEntry, RiotClient};
use bf_core::riot::rate_limiter::Clock;
use bf_core::testkit::{date_utc, load_test_static_data, FakeRiot, SimRiot, Synth, SynthGame, SynthOptions};
use bf_store::{Fetcher, FetcherConfig, Ingest, Settings, Store, StoreOptions, Work};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::watch;

const IRELIA: i64 = 39;
const KEY: &str = "RGAPI-00000000-test";

fn base_ms() -> i64 {
    date_utc(2026, 8, 5)
}

/// Epoch ms on tokio's (paused) clock, starting at `base_ms()`.
fn clock() -> Clock {
    let start = tokio::time::Instant::now();
    Arc::new(move || base_ms() as f64 + (tokio::time::Instant::now() - start).as_secs_f64() * 1000.0)
}

fn synth(n: u64) -> Vec<SynthGame> {
    let s = Synth::new(
        SynthOptions {
            platforms: vec!["EUW1".into()],
            players_per_platform: 400,
            timeline_share: 1.0,
            featured: Some((IRELIA, "MIDDLE".into())),
            featured_share: 0.2,
            ..Default::default()
        },
        load_test_static_data(),
    );
    (0..n).map(|i| s.game(i)).collect()
}

fn puuids(g: &SynthGame) -> Vec<String> {
    g.m["info"]["participants"].as_array().unwrap().iter().map(|p| p["puuid"].as_str().unwrap().to_string()).collect()
}

fn settings(me: Option<&str>) -> Settings {
    Settings { regions: vec![Platform::Euw1], my_platform: Platform::Euw1, puuid: me.map(String::from), tier_a: vec![IRELIA], timeline_sample: 0.0, ..Default::default() }
}

fn store(dir: &std::path::Path) -> Arc<Store> {
    Arc::new(Store::open(dir, load_test_static_data(), StoreOptions::default()).unwrap())
}

#[tokio::test(start_paused = true)]
async fn plan_follows_the_priority_order() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path());
    let games = synth(40);
    let g = games.iter().find(|g| g.m["info"]["participants"][2]["championId"] == IRELIA).unwrap();
    st.ingest(&[Ingest { m: &g.m, timeline: None, rank: RankBand::MasterPlus, me: None }]).unwrap();
    let irelia_player = g.m["info"]["participants"][2]["puuid"].as_str().unwrap().to_string();
    let cfg = FetcherConfig { widen_after_games: 1, ..Default::default() };
    let f = Fetcher::with_clock(Arc::new(FakeRiot::default()), st.clone(), settings(Some("me")), cfg, clock());
    let now = base_ms();
    // 1. The user's games.
    assert_eq!(f.plan(Routing::Europe, now).unwrap(), Work::MyGames { platform: Platform::Euw1, puuid: "me".into() });
    assert_eq!(f.plan(Routing::Asia, now).unwrap(), Work::Idle, "no KR in the regions");
    f.update_settings(Settings { puuid: None, riot_id: Some("Someone#EUW".into()), ..settings(None) });
    assert!(matches!(f.plan(Routing::Europe, now).unwrap(), Work::ResolveMe { .. }));
    // 2. Tier A: the player seen on Irelia.
    f.update_settings(settings(None));
    match f.plan(Routing::Europe, now).unwrap() {
        Work::Player { puuid, rank, label, .. } => {
            assert_eq!(puuid, irelia_player);
            assert_eq!(rank, RankBand::MasterPlus);
            assert_eq!(label, "tier A: Irelia MIDDLE");
        }
        w => panic!("{w:?}"),
    }
    st.mark_checked(&irelia_player, Platform::Euw1, RankBand::MasterPlus, now).unwrap();
    // 3. Thin pairs: the other nine players of that game.
    let others: HashSet<String> = puuids(g).into_iter().filter(|p| *p != irelia_player).collect();
    match f.plan(Routing::Europe, now).unwrap() {
        Work::Player { puuid, label, .. } => {
            assert!(others.contains(&puuid));
            assert!(label.starts_with("thin: "), "{label}");
        }
        w => panic!("{w:?}"),
    }
    for p in &others {
        st.mark_checked(p, Platform::Euw1, RankBand::MasterPlus, now).unwrap();
    }
    // Every pair is thin and Master+ has had its run (widen_after_games = 1): widen to Diamond's ladder.
    assert_eq!(f.plan(Routing::Europe, now).unwrap(), Work::Pool { platform: Platform::Euw1, rank: RankBand::Diamond });
    st.add_players(Platform::Euw1, RankBand::Diamond, &[("d1".into(), 50)], now).unwrap();
    match f.plan(Routing::Europe, now).unwrap() {
        Work::Player { puuid, rank, label, .. } => {
            assert_eq!((puuid.as_str(), rank), ("d1", RankBand::Diamond));
            assert!(label.starts_with("thin, widened to Diamond"), "{label}");
        }
        w => panic!("{w:?}"),
    }
    st.mark_checked("d1", Platform::Euw1, RankBand::Diamond, now).unwrap();
    // 4. Popular: the Master+ ladder (read first), then its players.
    f.update_settings(Settings { widen_to_diamond: false, ..settings(None) });
    assert_eq!(f.plan(Routing::Europe, now).unwrap(), Work::Pool { platform: Platform::Euw1, rank: RankBand::MasterPlus });
    st.add_players(Platform::Euw1, RankBand::MasterPlus, &[("m1".into(), 900), ("m2".into(), 1200)], now).unwrap();
    match f.plan(Routing::Europe, now).unwrap() {
        Work::Player { puuid, label, .. } => assert_eq!((puuid.as_str(), label.as_str()), ("m2", "popular"), "highest LP first"),
        w => panic!("{w:?}"),
    }
    st.mark_checked("m1", Platform::Euw1, RankBand::MasterPlus, now).unwrap();
    st.mark_checked("m2", Platform::Euw1, RankBand::MasterPlus, now).unwrap();
    assert_eq!(f.plan(Routing::Europe, now).unwrap(), Work::Idle);
    // Six hours later everyone is due again, tier A first.
    assert!(matches!(f.plan(Routing::Europe, now + 6 * 3_600_000 + 1).unwrap(), Work::Player { label, .. } if label.starts_with("tier A")));
}

/// A world of synthetic games served by FakeRiot (no budget), with a Master+ ladder.
fn fake_world(games: &[SynthGame]) -> Arc<FakeRiot> {
    let fake = FakeRiot::default();
    for g in games {
        fake.add(g.m.clone(), g.timeline.clone());
    }
    let mut ladder: Vec<String> = games.iter().flat_map(puuids).collect();
    ladder.sort();
    ladder.dedup();
    ladder.truncate(60);
    fake.ladders.lock().insert("EUW1:CHALLENGER".into(), ladder.into_iter().map(|p| LeagueEntry { puuid: p, league_points: 1000 }).collect());
    Arc::new(fake)
}

fn calls_of(fake: &FakeRiot, method: &str) -> Vec<Vec<Value>> {
    fake.calls.lock().iter().filter(|c| c.0 == method).map(|c| c.1.clone()).collect()
}

#[tokio::test(start_paused = true)]
async fn my_games_come_first_with_timelines_and_tier_a_games_get_theirs() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path());
    let games = synth(150);
    let me = puuids(&games[0])[3].clone();
    let fake = fake_world(&games);
    let f = Fetcher::with_clock(fake.clone(), st.clone(), settings(Some(&me)), FetcherConfig::default(), clock());
    let (_tx, rx) = watch::channel(Activity::Idle);
    let handles = f.spawn(rx);
    tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
    f.stop();
    for h in handles {
        h.await.unwrap();
    }
    let calls = fake.calls.lock().clone();
    // The very first request is the user's match list, and all their games (with timelines) come before the ladder.
    assert_eq!(calls[0].0, "matchIdsPage");
    assert_eq!(calls[0].1[1], json!(me));
    let first_ladder = calls.iter().position(|c| c.0 == "leagueEntries").unwrap();
    let mine: Vec<&String> = games.iter().filter(|g| puuids(g).contains(&me)).map(|g| &g.id).collect();
    assert!(!mine.is_empty());
    for id in &mine {
        let got = |m: &str| calls[..first_ladder].iter().any(|c| c.0 == m && c.1[1] == json!(id));
        assert!(got("match") && got("timeline"), "{id}");
    }
    // Timelines: every game with a tier A champion (Irelia, and the user's top champions, detected from their games)
    // and the user's own; none for the rest (sample 0).
    let tier_a = st.tier_a();
    assert!(tier_a.contains(&IRELIA) && tier_a.len() > 1, "{tier_a:?}");
    let timelines: HashSet<String> = calls_of(&fake, "timeline").iter().map(|c| c[1].as_str().unwrap().to_string()).collect();
    let fetched: Vec<String> = calls_of(&fake, "match").iter().map(|c| c[1].as_str().unwrap().to_string()).collect();
    assert!(fetched.len() > mine.len() + 20, "{} matches fetched", fetched.len());
    let by_id: HashMap<&str, &SynthGame> = games.iter().map(|g| (g.id.as_str(), g)).collect();
    for id in &fetched {
        let g = by_id[id.as_str()];
        let want = puuids(g).contains(&me) || g.m["info"]["participants"].as_array().unwrap().iter().any(|p| tier_a.contains(&p["championId"].as_i64().unwrap()));
        assert_eq!(timelines.contains(id), want, "{id}");
    }
    // No match is fetched twice, though players share games.
    let unique: HashSet<&String> = fetched.iter().collect();
    assert_eq!(unique.len(), fetched.len());
    let s = f.status();
    assert!(s.matches_total as usize >= fetched.len() - 5);
    assert!(s.requests_last_hour > 0);
}

#[tokio::test(start_paused = true)]
async fn champ_select_and_games_pause_fetching_and_the_new_game_comes_first_after() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path());
    let games = synth(120);
    let me = puuids(&games[0])[3].clone();
    let fake = fake_world(&games);
    let cfg = FetcherConfig { idle_ms: 5_000, ..Default::default() };
    let f = Fetcher::with_clock(fake.clone(), st.clone(), settings(Some(&me)), cfg, clock());
    let (tx, rx) = watch::channel(Activity::Busy);
    let handles = f.spawn(rx);
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert!(fake.calls.lock().len() > 0, "fetching while the PC is in use outside League");
    tx.send(Activity::ChampSelect).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let n = fake.calls.lock().len();
    assert_eq!(f.status().paused.as_deref(), Some("champ select"));
    assert!(!f.status().running);
    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    tx.send(Activity::InGame).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
    assert_eq!(fake.calls.lock().len(), n, "no request in champ select or in game");
    assert_eq!(f.status().paused.as_deref(), Some("in game"));
    // The game ends: the user's history is asked for first.
    tx.send(Activity::Idle).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let calls = fake.calls.lock().clone();
    assert!(calls.len() > n);
    assert_eq!(calls[n].0, "matchIdsPage");
    assert_eq!(calls[n].1[1], json!(me));
    assert_eq!(f.status().paused, None);
    f.stop();
    for h in handles {
        h.await.unwrap();
    }
}

/// The whole fetcher on the real client over a simulated Riot, with Irelia Companion spending the same key.
#[tokio::test(start_paused = true)]
async fn the_fetcher_shares_the_key_without_going_over() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path());
    let games = synth(400);
    let by_id: Arc<HashMap<String, (Value, Option<Value>)>> = Arc::new(games.iter().map(|g| (g.id.clone(), (g.m.clone(), g.timeline.clone()))).collect());
    let mut hist: HashMap<String, Vec<String>> = HashMap::new();
    for g in &games {
        for p in puuids(g) {
            hist.entry(p).or_default().push(g.id.clone());
        }
    }
    let mut ladder: Vec<String> = hist.keys().cloned().collect();
    ladder.sort();
    ladder.truncate(100);
    let hist = Arc::new(hist);
    let handler = {
        let by_id = by_id.clone();
        Box::new(move |_host: &str, path: &str| -> Option<Value> {
            let path = path.split('?').next().unwrap();
            if path.contains("challengerleagues") {
                return Some(json!({ "entries": ladder.iter().map(|p| json!({ "puuid": p, "leaguePoints": 1000 })).collect::<Vec<_>>() }));
            }
            if path.contains("leagues/by-queue") {
                return Some(json!({ "entries": [] }));
            }
            if let Some(rest) = path.strip_prefix("/lol/match/v5/matches/by-puuid/") {
                let puuid = rest.split('/').next().unwrap();
                return Some(json!(hist.get(puuid).cloned().unwrap_or_default().into_iter().rev().take(20).collect::<Vec<_>>()));
            }
            if let Some(id) = path.strip_prefix("/lol/match/v5/matches/") {
                return match id.strip_suffix("/timeline") {
                    Some(id) => by_id.get(id).and_then(|x| x.1.clone()),
                    None => by_id.get(id).map(|x| x.0.clone()),
                };
            }
            None
        })
    };
    let clk = clock();
    // The simulator's clock is the fetcher's (epoch ms), so its buckets line up with the client's view.
    let sim = SimRiot::new(clk.clone(), "20:1,100:120", KEY, handler);
    sim.add_foreign("europe.api.riotgames.com", 3000, base_ms(), base_ms() + 1_800_000);
    let pacing = bf_core::config::PacingOptions { safety: 0.9, burst: 3.0, concurrency: 3, slowdown: 0.8, recovery_ms: 60_000.0 };
    let api = Arc::new(RiotClient::with_transport(KEY, parse_limits(Some("20:1,100:120")), pacing, sim, clk.clone()));
    let s = Settings { timeline_sample: 0.1, ..settings(None) };
    let f = Fetcher::with_clock(api.clone(), st.clone(), s, FetcherConfig::default(), clk);
    let (_tx, rx) = watch::channel(Activity::Idle);
    let handles = f.spawn(rx);
    tokio::time::sleep(std::time::Duration::from_secs(900)).await;
    let status = f.status();
    tokio::time::sleep(std::time::Duration::from_secs(900)).await;
    f.stop();
    for h in handles {
        h.await.unwrap();
    }
    let calls = api.transport().calls();
    let europe: Vec<_> = calls.iter().filter(|c| c.host == "europe.api.riotgames.com").collect();
    assert_eq!(calls.iter().filter(|c| c.status == 429).count(), 0, "no 429 for us");
    assert_eq!(api.transport().foreign().1, 0, "none for Irelia Companion either");
    assert!(api.transport().peak("europe.api.riotgames.com") <= 100);
    // 30 minutes at ~52 requests per 2 minutes left by the other app.
    assert!((550..=900).contains(&europe.len()), "{} europe requests", europe.len());
    let b = status.budgets.iter().find(|b| b.scope == "EUROPE").unwrap();
    assert!(b.shared && b.used >= b.ours, "{b:?}");
    assert!(status.budgets.iter().any(|b| b.scope == "euw1"), "league-v4 has its own budget");
    let (counted, _, validation, _) = st.seen_counts().unwrap();
    assert!(counted + validation > 300, "{counted} games");
    eprintln!("fetcher, 30 min shared key: {} europe requests, {} games, budget {b:?}", europe.len(), counted + validation);
}

#[tokio::test(start_paused = true)]
async fn a_rejected_key_stops_fetching_until_a_new_key_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path());
    let clk = clock();
    let sim = SimRiot::new(clk.clone(), "20:1,100:120", KEY, Box::new(|_, _| Some(json!([]))));
    let pacing = bf_core::config::PacingOptions { safety: 0.9, burst: 3.0, concurrency: 3, slowdown: 0.8, recovery_ms: 60_000.0 };
    let api = Arc::new(RiotClient::with_transport("RGAPI-99999999-test", parse_limits(Some("20:1,100:120")), pacing, sim, clk.clone()));
    let f = Fetcher::with_clock(api.clone(), st, settings(Some("me")), FetcherConfig::default(), clk);
    let (_tx, rx) = watch::channel(Activity::Idle);
    let handles = f.spawn(rx);
    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    let s = f.status();
    assert_eq!(s.paused.as_deref(), Some("key rejected"));
    let e = s.key_error.unwrap();
    assert!(e.contains("developer.riotgames.com") && !e.contains("99999999"), "{e}");
    let n = api.transport().calls().len();
    assert!(n <= 2, "{n} requests with a rejected key");
    api.set_api_key(KEY);
    f.set_key_present(true);
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    assert!(api.transport().calls().len() > n);
    assert_eq!(f.status().key_error, None);
    f.stop();
    for h in handles {
        h.await.unwrap();
    }
}
