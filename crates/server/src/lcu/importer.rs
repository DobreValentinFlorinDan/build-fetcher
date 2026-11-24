//! From irelia-companion's `server/lcu/importer.ts`, for any champion: one-click import of the build shown into the
//! League client, a rune page and an item set. Only on the user's click; it replaces only what it created itself
//! (the page and the sets named "Build Fetcher…"), and a full set of rune pages is never touched.

use bf_core::build::{BuildRecommendation, OptionKind, RankedOption, Role};
use serde::Serialize;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;

pub const PAGE_NAME: &str = "Build Fetcher";
pub const SET_PREFIX: &str = "Build Fetcher";
const SUMMONERS_RIFT: i64 = 11;
const POTION: i64 = 2003;
const CONTROL_WARD: i64 = 2055;

pub type SendFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + 'a>>;

/// The League client, as far as the import needs it (LcuConnector, or a fake in tests).
pub trait LcuApi: Send + Sync {
    fn send<'a>(&'a self, method: &'a str, path: &'a str, body: Option<&'a Value>) -> SendFuture<'a>;
}

fn options(build: &BuildRecommendation, kind: OptionKind) -> &[RankedOption] {
    build.sections.iter().find(|s| s.kind == kind).map(|s| s.options.as_slice()).unwrap_or(&[])
}

fn ids(s: &str) -> Option<Vec<i64>> {
    s.split(',').map(|x| x.trim().parse().ok()).collect()
}

/// A RunePage key (`style:perks/substyle:perks/shards`) → (primary style, sub style, the 9 selected perk ids).
pub fn parse_rune_page(key: &str) -> Option<(i64, i64, Vec<i64>)> {
    let mut parts = key.split('/');
    let (style, perks) = parts.next()?.split_once(':')?;
    let (sub, sub_perks) = parts.next()?.split_once(':')?;
    let shards = ids(parts.next()?)?;
    let (perks, sub_perks) = (ids(perks)?, ids(sub_perks)?);
    if perks.len() != 4 || sub_perks.len() != 2 || shards.len() != 3 || parts.next().is_some() {
        return None;
    }
    Some((style.parse().ok()?, sub.parse().ok()?, [perks, sub_perks, shards].concat()))
}

/// The best rune page of the build, as the client's POST /lol-perks/v1/pages takes it.
pub fn rune_page_body(build: &BuildRecommendation) -> Option<Value> {
    let best = options(build, OptionKind::RunePage).first()?;
    let (primary, sub, perks) = parse_rune_page(&best.option.key)?;
    Some(json!({ "name": PAGE_NAME, "primaryStyleId": primary, "subStyleId": sub, "selectedPerkIds": perks, "current": true }))
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Top => "Top",
        Role::Jungle => "Jungle",
        Role::Middle => "Mid",
        Role::Bottom => "Bot",
        Role::Utility => "Support",
    }
}

pub fn set_title(champion_name: &str, role: Role) -> String {
    format!("{SET_PREFIX} · {champion_name} {}", role_name(role))
}

/// The item set: the best start, the best core in order, then the alternatives the build ranks (boots, other first
/// items, late items), best first, so the shop shows the choices rather than one answer.
pub fn item_set_body(build: &BuildRecommendation, title: &str, uid: &str) -> Value {
    let block = |kind: &str, ids: &[i64]| {
        let mut counts: indexmap::IndexMap<i64, i64> = indexmap::IndexMap::new();
        for id in ids {
            *counts.entry(*id).or_insert(0) += 1;
        }
        json!({ "type": kind, "items": counts.iter().map(|(id, c)| json!({ "id": id.to_string(), "count": c })).collect::<Vec<_>>() })
    };
    let top = |kind: OptionKind, n: usize| options(build, kind).iter().take(n).flat_map(|o| o.option.item_ids()).collect::<Vec<_>>();
    let core = top(OptionKind::CoreItems, 1);
    let core = if core.is_empty() { top(OptionKind::FirstItem, 1) } else { core };
    let mut blocks: Vec<Value> = vec![];
    let mut push = |label: String, items: Vec<i64>| {
        if !items.is_empty() {
            blocks.push(block(&label, &items));
        }
    };
    push("Starting items".into(), top(OptionKind::StartingItems, 1));
    push("Core items, in order".into(), core.clone());
    push("Boots (best first)".into(), top(OptionKind::Boots, 2));
    let other_first: Vec<i64> = top(OptionKind::FirstItem, 4).into_iter().filter(|i| core.first() != Some(i)).take(3).collect();
    push("Other first items".into(), other_first);
    push("Late items (best first)".into(), top(OptionKind::LateItem, 5));
    push("Potions and wards".into(), vec![POTION, CONTROL_WARD]);
    json!({
        "uid": uid, "title": title, "type": "custom", "map": "any", "mode": "any", "startedFrom": "blank", "sortrank": 0,
        "associatedChampions": [build.champion], "associatedMaps": [SUMMONERS_RIFT], "preferredItemSlots": [], "blocks": blocks,
    })
}

/// One part of the import: done, or why not.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The page replaced (or the set) one Build Fetcher made before.
    pub replaced: bool,
}

impl PartResult {
    fn failed(e: impl ToString) -> PartResult {
        PartResult { ok: false, error: Some(e.to_string()), replaced: false }
    }
}

/// What the app created in the client, so the next import replaces it (kept in memory for the session).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Owned {
    pub rune_page_id: Option<i64>,
    pub item_set_uid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub rune_page: PartResult,
    pub item_set: PartResult,
    pub title: String,
    #[serde(skip)]
    pub owned: Owned,
}

async fn import_runes(api: &dyn LcuApi, build: &BuildRecommendation, saved: Option<i64>) -> anyhow::Result<(PartResult, Option<i64>)> {
    let Some(body) = rune_page_body(build) else { return Ok((PartResult::failed("This build has no complete rune page yet."), None)) };
    let pages = api.send("GET", "/lol-perks/v1/pages", None).await?;
    let pages: Vec<Value> = pages.as_array().cloned().unwrap_or_default();
    let editable: Vec<&Value> = pages.iter().filter(|p| p["isEditable"] != false && p["isDeletable"] != false).collect();
    let ours = editable
        .iter()
        .find(|p| saved.is_some() && p["id"].as_i64() == saved && p["name"] == PAGE_NAME)
        .or_else(|| editable.iter().find(|p| p["name"] == PAGE_NAME))
        .copied();
    if let Some(o) = ours {
        api.send("DELETE", &format!("/lol-perks/v1/pages/{}", o["id"].as_i64().unwrap_or(0)), None).await?;
    } else {
        let inventory = api.send("GET", "/lol-perks/v1/inventory", None).await?;
        if let Some(owned) = inventory["ownedPageCount"].as_i64() {
            if editable.len() as i64 >= owned {
                return Ok((
                    PartResult::failed(format!(
                        "No free rune page ({} of {owned} in use). Delete one in the client, or rename one to \"{PAGE_NAME}\" to let the app use it.",
                        editable.len()
                    )),
                    None,
                ));
            }
        }
    }
    let created = api.send("POST", "/lol-perks/v1/pages", Some(&body)).await?;
    match created["id"].as_i64().filter(|id| *id != 0) {
        Some(id) => Ok((PartResult { ok: true, error: None, replaced: ours.is_some() }, Some(id))),
        None => Ok((PartResult::failed("The League client did not create the rune page."), None)),
    }
}

async fn import_items(api: &dyn LcuApi, build: &BuildRecommendation, title: &str, saved: Option<&str>) -> anyhow::Result<(PartResult, Option<String>)> {
    // The item-set endpoint is keyed by the signed-in user's own summoner id (never anyone else's).
    let me = api.send("GET", "/lol-summoner/v1/current-summoner", None).await?;
    let Some(sid) = me["summonerId"].as_i64().filter(|s| *s != 0) else {
        return Ok((PartResult::failed("The League client did not say who is signed in."), None));
    };
    let path = format!("/lol-item-sets/v1/item-sets/{sid}/sets");
    let mut current = api.send("GET", &path, None).await?;
    if current.is_null() {
        current = json!({ "itemSets": [] });
    }
    let sets: Vec<Value> = current["itemSets"].as_array().cloned().unwrap_or_default();
    let is_ours = |s: &Value| (saved.is_some() && s["uid"].as_str() == saved) || s["title"].as_str().is_some_and(|t| t.starts_with(SET_PREFIX));
    let replaced = sets.iter().any(is_ours);
    let set = item_set_body(build, title, &bf_core::util::random_uuid());
    let mut kept: Vec<Value> = sets.into_iter().filter(|s| !is_ours(s)).collect();
    kept.push(set.clone());
    let mut body = current.clone();
    if let Some(o) = body.as_object_mut() {
        o.insert("itemSets".into(), Value::Array(kept));
        o.insert("timestamp".into(), json!(bf_core::util::now_ms()));
    }
    api.send("PUT", &path, Some(&body)).await?;
    Ok((PartResult { ok: true, error: None, replaced }, set["uid"].as_str().map(String::from)))
}

/// Each part succeeds or fails on its own, with the reason.
pub async fn import_build(api: &dyn LcuApi, build: &BuildRecommendation, champion_name: &str, owned: &Owned) -> ImportResult {
    let title = set_title(champion_name, build.role);
    let (rune_page, page_id) = import_runes(api, build, owned.rune_page_id).await.unwrap_or_else(|e| (PartResult::failed(e), None));
    let (item_set, uid) = import_items(api, build, &title, owned.item_set_uid.as_deref()).await.unwrap_or_else(|e| (PartResult::failed(e), None));
    let owned = Owned { rune_page_id: page_id.or(owned.rune_page_id), item_set_uid: uid.or_else(|| owned.item_set_uid.clone()) };
    ImportResult { rune_page, item_set, title, owned }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bf_core::build::{BuildOption, DraftContext, Tier};
    use bf_core::build::BuildSource;
    use parking_lot::Mutex;

    /// The League client's rune pages and item sets, in memory; records every write.
    pub struct FakeLcu {
        pub pages: Mutex<Vec<Value>>,
        pub sets: Mutex<Value>,
        pub writes: Mutex<Vec<String>>,
        pub owned_pages: i64,
        next: Mutex<i64>,
        pub fail_items: bool,
    }

    pub fn fake(pages: Vec<Value>, owned_pages: i64, sets: Vec<Value>) -> FakeLcu {
        FakeLcu {
            pages: Mutex::new(pages),
            sets: Mutex::new(json!({ "accountId": 7, "itemSets": sets, "timestamp": 1 })),
            writes: Mutex::new(vec![]),
            owned_pages,
            next: Mutex::new(900),
            fail_items: false,
        }
    }

    impl LcuApi for FakeLcu {
        fn send<'a>(&'a self, method: &'a str, path: &'a str, body: Option<&'a Value>) -> SendFuture<'a> {
            Box::pin(async move {
                if self.fail_items && path.contains("item-sets") {
                    anyhow::bail!("HTTP 500 PUT");
                }
                if method != "GET" {
                    self.writes.lock().push(format!("{method} {path}"));
                }
                match (method, path) {
                    ("GET", "/lol-perks/v1/pages") => Ok(Value::Array(self.pages.lock().clone())),
                    ("GET", "/lol-perks/v1/inventory") => Ok(json!({ "ownedPageCount": self.owned_pages })),
                    ("DELETE", p) if p.starts_with("/lol-perks/v1/pages/") => {
                        let id: i64 = p.rsplit('/').next().unwrap().parse().unwrap();
                        self.pages.lock().retain(|x| x["id"].as_i64() != Some(id));
                        Ok(Value::Null)
                    }
                    ("POST", "/lol-perks/v1/pages") => {
                        let mut n = self.next.lock();
                        let mut page = body.unwrap().clone();
                        page["id"] = json!(*n);
                        page["isEditable"] = json!(true);
                        *n += 1;
                        self.pages.lock().push(page.clone());
                        Ok(page)
                    }
                    ("GET", "/lol-summoner/v1/current-summoner") => Ok(json!({ "summonerId": 42, "puuid": "own-puuid" })),
                    ("GET", "/lol-item-sets/v1/item-sets/42/sets") => Ok(self.sets.lock().clone()),
                    ("PUT", "/lol-item-sets/v1/item-sets/42/sets") => {
                        *self.sets.lock() = body.unwrap().clone();
                        Ok(Value::Null)
                    }
                    _ => anyhow::bail!("HTTP 404 {method} {path}"),
                }
            })
        }
    }

    fn opt(option: BuildOption) -> RankedOption {
        RankedOption { label: option.key.clone(), option, games: 50, wins: 28, weighted_games: 40.0, win_rate: 0.55, wpa: 0.02, predicted_win: None, basis: bf_core::build::Basis::Lane, thin: false }
    }

    fn build(champion: i64, role: Role) -> BuildRecommendation {
        let sec = |kind, options: Vec<BuildOption>| bf_core::build::Section { kind, options: options.into_iter().map(opt).collect() };
        BuildRecommendation {
            champion,
            role,
            patch: None,
            tier: Tier::B,
            lane_opponent: None,
            enemy_profile: None,
            games: 500,
            sections: vec![
                sec(OptionKind::RunePage, vec![BuildOption::rune_page(8100, &[8112, 8139, 8140, 8135], 8300, &[8304, 8347], &[5008, 5008, 5001])]),
                sec(OptionKind::StartingItems, vec![BuildOption::starting_items(&[1056, 2003, 2003]).unwrap()]),
                sec(OptionKind::FirstItem, vec![BuildOption::first_item(6655), BuildOption::first_item(4645), BuildOption::first_item(3157)]),
                sec(OptionKind::CoreItems, vec![BuildOption::core_items(&[6655, 3020, 3089]).unwrap()]),
                sec(OptionKind::Boots, vec![BuildOption::boots(3020), BuildOption::boots(3158), BuildOption::boots(3111)]),
                sec(OptionKind::LateItem, vec![BuildOption::late_item(3135), BuildOption::late_item(3157)]),
            ],
            model: None,
            notes: vec![],
        }
    }

    fn titles(set: &Value) -> Vec<String> {
        set["blocks"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap().to_string()).collect()
    }
    fn block_ids(b: &Value) -> Vec<String> {
        b["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
    }
    fn names(f: &FakeLcu) -> Vec<String> {
        f.pages.lock().iter().map(|p| p["name"].as_str().unwrap().to_string()).collect()
    }
    fn set_titles(f: &FakeLcu) -> Vec<String> {
        f.sets.lock()["itemSets"].as_array().unwrap().iter().map(|b| b["title"].as_str().unwrap_or("").to_string()).collect()
    }

    #[test]
    fn bodies_for_any_champion() {
        let b = build(103, Role::Middle);
        let page = rune_page_body(&b).unwrap();
        assert_eq!(page["primaryStyleId"], json!(8100));
        assert_eq!(page["subStyleId"], json!(8300));
        assert_eq!(page["selectedPerkIds"], json!([8112, 8139, 8140, 8135, 8304, 8347, 5008, 5008, 5001]));
        let set = item_set_body(&b, &set_title("Ahri", Role::Middle), "u1");
        assert_eq!(set["title"], "Build Fetcher · Ahri Mid");
        assert_eq!(set["associatedChampions"], json!([103]));
        assert_eq!(set["associatedMaps"], json!([11]));
        assert_eq!(titles(&set), vec!["Starting items", "Core items, in order", "Boots (best first)", "Other first items", "Late items (best first)", "Potions and wards"]);
        assert_eq!(set["blocks"][0]["items"], json!([{ "id": "1056", "count": 1 }, { "id": "2003", "count": 2 }]));
        assert_eq!(block_ids(&set["blocks"][1]), vec!["6655", "3020", "3089"]);
        assert_eq!(block_ids(&set["blocks"][2]), vec!["3020", "3158"]);
        assert_eq!(block_ids(&set["blocks"][3]), vec!["4645", "3157"], "the core's first item isn't repeated");
        assert!(parse_rune_page("8000:1,2,3/8400:4,5/6,7,8").is_none(), "three primary runes");
        assert!(parse_rune_page("x").is_none());
    }

    #[test]
    fn every_champion_in_data_dragon_gets_a_valid_import_from_the_stub() {
        // Any champion: the stub BuildSource's build for each of the 173 champions makes a complete page and a set.
        let data = bf_core::testkit::load_test_static_data();
        let slot = crate::static_data::empty_slot();
        *slot.write() = Some(data.clone());
        let stub = crate::fakes::StubBuildSource { data: slot };
        for c in data.champions.values() {
            let rec = stub.recommend(&DraftContext { champion: c.id, ..Default::default() }).unwrap();
            let page = rune_page_body(&rec).unwrap_or_else(|| panic!("{} has no page", c.name));
            for id in page["selectedPerkIds"].as_array().unwrap().iter().take(6) {
                assert!(data.rune_ids.contains(&id.as_i64().unwrap()), "{}: rune {id} not in Data Dragon", c.name);
            }
            let set = item_set_body(&rec, &set_title(&c.name, rec.role), "u");
            assert_eq!(set["associatedChampions"], json!([c.id]));
            for b in set["blocks"].as_array().unwrap() {
                for id in block_ids(b) {
                    assert!(data.item(id.parse().unwrap()).is_some_and(|i| i.on_summoners_rift), "{}: item {id}", c.name);
                }
            }
        }
    }

    #[tokio::test]
    async fn first_import_keeps_the_users_pages_and_sets() {
        let f = fake(vec![json!({ "id": 1, "name": "My page" }), json!({ "id": 2, "name": "Default", "isEditable": false })], 5, vec![json!({ "uid": "mine", "title": "My set" })]);
        let r = import_build(&f, &build(266, Role::Top), "Aatrox", &Owned::default()).await;
        assert!(r.rune_page.ok && !r.rune_page.replaced);
        assert!(r.item_set.ok && !r.item_set.replaced);
        assert_eq!(r.title, "Build Fetcher · Aatrox Top");
        assert_eq!(names(&f), vec!["My page", "Default", PAGE_NAME]);
        assert_eq!(set_titles(&f), vec!["My set", "Build Fetcher · Aatrox Top"]);
        assert_eq!(f.sets.lock()["accountId"], json!(7), "the rest of the client's document is kept");
        assert_eq!(*f.writes.lock(), vec!["POST /lol-perks/v1/pages", "PUT /lol-item-sets/v1/item-sets/42/sets"]);
        assert_eq!(r.owned.rune_page_id, f.pages.lock()[2]["id"].as_i64());
    }

    #[tokio::test]
    async fn importing_another_champion_replaces_only_its_own() {
        let f = fake(
            vec![json!({ "id": 1, "name": "My page" }), json!({ "id": 77, "name": PAGE_NAME })],
            5,
            vec![json!({ "uid": "mine", "title": "My set" }), json!({ "uid": "old", "title": "Build Fetcher · Ahri Mid" })],
        );
        let owned = Owned { rune_page_id: Some(77), item_set_uid: Some("old".into()) };
        let r = import_build(&f, &build(222, Role::Bottom), "Jinx", &owned).await;
        assert!(r.rune_page.replaced && r.item_set.replaced);
        assert_eq!(names(&f), vec!["My page", PAGE_NAME]);
        assert_ne!(f.pages.lock()[1]["id"], json!(77));
        assert_eq!(set_titles(&f), vec!["My set", "Build Fetcher · Jinx Bot"]);
        assert_eq!(*f.writes.lock(), vec!["DELETE /lol-perks/v1/pages/77", "POST /lol-perks/v1/pages", "PUT /lol-item-sets/v1/item-sets/42/sets"]);
    }

    #[tokio::test]
    async fn no_free_rune_page_touches_none() {
        let f = fake(vec![json!({ "id": 1, "name": "A" }), json!({ "id": 2, "name": "B" })], 2, vec![]);
        let r = import_build(&f, &build(12, Role::Utility), "Alistar", &Owned::default()).await;
        assert!(!r.rune_page.ok);
        assert!(r.rune_page.error.as_deref().unwrap().contains("No free rune page (2 of 2 in use)"));
        assert!(r.item_set.ok);
        assert_eq!(names(&f), vec!["A", "B"]);
        assert!(!f.writes.lock().iter().any(|w| w.starts_with("DELETE") || w.starts_with("POST")));
    }

    #[tokio::test]
    async fn a_client_error_on_one_part_leaves_the_other() {
        let mut f = fake(vec![], 5, vec![]);
        f.fail_items = true;
        let r = import_build(&f, &build(64, Role::Jungle), "Lee Sin", &Owned { rune_page_id: None, item_set_uid: Some("keep".into()) }).await;
        assert!(r.rune_page.ok);
        assert!(!r.item_set.ok);
        assert_eq!(r.owned.item_set_uid.as_deref(), Some("keep"));
    }
}
