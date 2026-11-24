//! Port of `server/lcu/importer.ts` (Stage C1): one-click import of the build shown into the League client: a rune
//! page and an item set. Only on your click, only in champ select; it replaces only what it created itself.

use irelia::types::BuildRecommendation;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;

pub const PAGE_NAME: &str = "Irelia Companion";
pub const SET_PREFIX: &str = "Irelia Companion";
const IRELIA: i64 = 39;
const SUMMONERS_RIFT: i64 = 11;

pub type SendFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + 'a>>;

/// The League client, as far as the import needs it (LcuConnector::send, or a fake in tests).
pub trait LcuApi: Sync {
    fn send<'a>(&'a self, method: &'a str, path: &'a str, body: Option<&'a Value>) -> SendFuture<'a>;
}

pub fn rune_page_body(build: &BuildRecommendation) -> Option<Value> {
    let r = &build.runes.as_ref()?.value;
    if r.perks.len() != 6 || r.stat_perks.len() != 3 {
        return None;
    }
    let mut ids = r.perks.clone();
    ids.extend(r.stat_perks.iter());
    Some(json!({ "name": PAGE_NAME, "primaryStyleId": r.primary_style, "subStyleId": r.sub_style, "selectedPerkIds": ids, "current": true }))
}

/// What did best against the lane opponent (MatchupBest as JSON), for the "vs" blocks.
pub struct VersusBlocks {
    pub name: String,
    pub best: Value,
}

fn num(v: &Value) -> String {
    irelia::js::num_to_string(v.as_f64().unwrap_or(f64::NAN))
}

pub fn item_set_body(build: &BuildRecommendation, title: &str, uid: &str, vs: Option<&VersusBlocks>) -> Value {
    let block = |kind: &str, ids: &[i64]| {
        let mut counts: indexmap::IndexMap<i64, i64> = indexmap::IndexMap::new();
        for id in ids {
            *counts.entry(*id).or_insert(0) += 1;
        }
        json!({ "type": kind, "items": counts.iter().map(|(id, c)| json!({ "id": id.to_string(), "count": c })).collect::<Vec<_>>() })
    };
    let top = |opts: &[irelia::types::Opt<i64>], n: usize| opts.iter().take(n).map(|o| o.value).collect::<Vec<_>>();
    let start: Vec<i64> = build.starting_items.first().map(|o| o.value.clone()).unwrap_or_default();
    let core_items: Vec<i64> = build.core_build.first().map(|o| o.value.clone()).unwrap_or_default();
    let after = build.boots_after.flatten().unwrap_or(core_items.len() as f64);
    let at = irelia::model::logistic::js_max(0.0, irelia::model::logistic::js_min(core_items.len() as f64, after));
    let at = if at.is_nan() { 0 } else { at as usize };
    let mut core: Vec<i64> = core_items[..at].to_vec();
    core.extend(top(&build.boots, 1));
    core.extend(core_items[at..].iter());
    let vs_start = vs.map(|v| &v.best["start"]).filter(|s| !s.is_null());
    let vs_first: Vec<&Value> = vs.and_then(|v| v.best["first"].as_array()).map(|a| a.iter().collect()).unwrap_or_default();
    let ids = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect::<Vec<_>>()).unwrap_or_default();
    let mut blocks: Vec<Value> = Vec::new();
    if !start.is_empty() {
        blocks.push(block("Start", &start));
    }
    if let (Some(s), Some(v)) = (vs_start, vs) {
        blocks.push(block(&format!("vs {}: best start ({} games)", v.name, num(&s["games"])), &ids(&s["value"])));
    }
    if !core.is_empty() {
        blocks.push(block("Core and boots, in order", &core));
    }
    if let (false, Some(v)) = (vs_first.is_empty(), vs) {
        let games = vs_first.iter().map(|o| num(&o["games"])).collect::<Vec<_>>().join(" · ");
        blocks.push(block(&format!("vs {}: best first item ({} games)", v.name, games), &vs_first.iter().filter_map(|o| o["value"].as_i64()).collect::<Vec<_>>()));
    }
    for (opts, label) in [(&build.situational.slot4, "4th item (best first)"), (&build.situational.slot5, "5th item (best first)"), (&build.situational.slot6, "6th item (best first)")] {
        if !opts.is_empty() {
            blocks.push(block(label, &top(opts, 3)));
        }
    }
    blocks.push(block("Vision and potions", &[2055, 2003]));
    json!({
        "uid": uid, "title": title, "type": "custom", "map": "any", "mode": "any", "startedFrom": "blank", "sortrank": 0,
        "associatedChampions": [IRELIA], "associatedMaps": [SUMMONERS_RIFT], "preferredItemSlots": [], "blocks": blocks,
    })
}

async fn import_runes(api: &dyn LcuApi, build: &BuildRecommendation, saved: Option<i64>) -> anyhow::Result<Value> {
    let Some(body) = rune_page_body(build) else { return Ok(json!({ "ok": false, "error": "This build has no complete rune page yet." })) };
    let pages = api.send("GET", "/lol-perks/v1/pages", None).await?;
    let pages: Vec<Value> = pages.as_array().cloned().unwrap_or_default();
    let editable: Vec<&Value> = pages.iter().filter(|p| p["isEditable"] != false && p["isDeletable"] != false).collect();
    let ours = editable
        .iter()
        .find(|p| p["id"].as_i64() == saved && saved.is_some() && p["name"] == PAGE_NAME)
        .or_else(|| editable.iter().find(|p| p["name"] == PAGE_NAME))
        .copied();
    if let Some(o) = ours {
        api.send("DELETE", &format!("/lol-perks/v1/pages/{}", num(&o["id"])), None).await?;
    } else {
        let inventory = api.send("GET", "/lol-perks/v1/inventory", None).await?;
        if let Some(owned) = inventory["ownedPageCount"].as_f64() {
            if editable.len() as f64 >= owned {
                return Ok(json!({ "ok": false, "error": format!(
                    "No free rune page ({} of {} in use). Delete one in the client, or rename one to \"{}\" to let the app use it.",
                    editable.len(), irelia::js::num_to_string(owned), PAGE_NAME) }));
            }
        }
    }
    let created = api.send("POST", "/lol-perks/v1/pages", Some(&body)).await?;
    match created["id"].as_i64().filter(|id| *id != 0) {
        Some(id) => Ok(json!({ "ok": true, "id": id, "replaced": ours.is_some() })),
        None => Ok(json!({ "ok": false, "error": "The League client did not create the rune page." })),
    }
}

async fn import_items(api: &dyn LcuApi, build: &BuildRecommendation, title: &str, saved: Option<&str>, vs: Option<&VersusBlocks>) -> anyhow::Result<Value> {
    let me = api.send("GET", "/lol-summoner/v1/current-summoner", None).await?;
    let Some(sid) = me["summonerId"].as_f64().filter(|s| *s != 0.0) else {
        return Ok(json!({ "ok": false, "error": "The League client did not say who is signed in." }));
    };
    let path = format!("/lol-item-sets/v1/item-sets/{}/sets", irelia::js::num_to_string(sid));
    let mut current = api.send("GET", &path, None).await?;
    if current.is_null() {
        current = json!({ "itemSets": [] });
    }
    let sets: Vec<Value> = current["itemSets"].as_array().cloned().unwrap_or_default();
    let is_ours = |s: &Value| (saved.is_some() && s["uid"].as_str() == saved) || s["title"].as_str().map(|t| t.starts_with(SET_PREFIX)).unwrap_or(false);
    let replaced = sets.iter().any(is_ours);
    let set = item_set_body(build, title, &irelia::util::random_uuid(), vs);
    let mut kept: Vec<Value> = sets.into_iter().filter(|s| !is_ours(s)).collect();
    kept.push(set.clone());
    let mut body = current.clone();
    if let Some(o) = body.as_object_mut() {
        o.insert("itemSets".into(), Value::Array(kept));
        o.insert("timestamp".into(), json!(irelia::util::now_ms()));
    }
    api.send("PUT", &path, Some(&body)).await?;
    Ok(json!({ "ok": true, "uid": set["uid"], "title": title, "replaced": replaced }))
}

pub struct ImportOptions {
    pub lane: &'static str,
    pub opponent_name: Option<String>,
    pub rune_page_id: Option<i64>,
    pub item_set_uid: Option<String>,
    pub best: Option<Value>,
}

/// ImportResult as JSON: each part succeeds or fails on its own, with the reason.
pub async fn import_build(api: &dyn LcuApi, build: &BuildRecommendation, o: &ImportOptions) -> Value {
    let title = format!(
        "{} · {}{}",
        SET_PREFIX,
        if o.lane == "MIDDLE" { "Mid" } else { "Top" },
        o.opponent_name.as_ref().map(|n| format!(" vs {n}")).unwrap_or_default()
    );
    let rune_page = import_runes(api, build, o.rune_page_id).await.unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }));
    let vs = match (&o.best, &o.opponent_name) {
        (Some(b), Some(n)) if !b.is_null() => Some(VersusBlocks { name: n.clone(), best: b.clone() }),
        _ => None,
    };
    let item_set = import_items(api, build, &title, o.item_set_uid.as_deref(), vs.as_ref()).await.unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }));
    let mut owned = serde_json::Map::new();
    let rp = if rune_page["ok"] == true { rune_page["id"].as_i64() } else { o.rune_page_id };
    let iu = if item_set["ok"] == true { item_set["uid"].as_str().map(String::from) } else { o.item_set_uid.clone() };
    if let Some(r) = rp {
        owned.insert("runePageId".into(), json!(r));
    }
    if let Some(u) = iu {
        owned.insert("itemSetUid".into(), json!(u));
    }
    json!({ "runePage": rune_page, "itemSet": item_set, "owned": owned })
}

#[cfg(test)]
mod tests {
    //! Port of server/lcu/importer.test.ts.
    use super::*;
    use parking_lot::Mutex;

    fn opt<T: serde::Serialize>(v: T) -> Value {
        json!({ "value": v, "games": 50, "rawGames": 50, "winRate": 0.55, "score": 0.5 })
    }
    fn build_json(boots_after: Option<f64>) -> BuildRecommendation {
        let mut b = json!({
            "patch": "16.19", "ddragonVersion": "16.19.1", "enemyChampionId": null, "lane": "TOP",
            "sample": { "matchupGames": 0, "totalGames": 0, "patches": [], "usedFallback": false, "laneGames": 0, "otherLaneGames": 0, "usedLaneFallback": false },
            "winRate": null,
            "runes": opt(json!({ "primaryStyle": 8000, "subStyle": 8400, "perks": [8010, 9111, 9104, 8299, 8444, 8242], "statPerks": [5008, 5008, 5001] })),
            "summoners": null,
            "startingItems": [opt(vec![1055, 2003, 2003])],
            "coreBuild": [opt(vec![3153, 3078, 6333])],
            "boots": [opt(3047), opt(3111)],
            "situational": { "slot4": [opt(3053), opt(6609), opt(3026), opt(3065)], "slot5": [opt(3026)], "slot6": [] },
            "skills": null,
            "validation": { "currentPatch": "16.19", "freshGames": 0, "previousPatchGames": 0, "staleDiscarded": 0, "invalidDiscarded": 0, "invalidReasons": {}, "conflictsRejected": [] },
        });
        if let Some(a) = boots_after {
            b["bootsAfter"] = json!(a);
        }
        serde_json::from_value(b).expect("build")
    }

    struct Fake {
        pages: Mutex<Vec<Value>>,
        sets: Mutex<Value>,
        writes: Mutex<Vec<String>>,
        owned_pages: f64,
        next: Mutex<i64>,
        fail_items: bool,
    }
    fn fake(pages: Vec<Value>, owned_pages: f64, sets: Vec<Value>) -> Fake {
        Fake {
            pages: Mutex::new(pages),
            sets: Mutex::new(json!({ "accountId": 7, "itemSets": sets, "timestamp": 1 })),
            writes: Mutex::new(vec![]),
            owned_pages,
            next: Mutex::new(900),
            fail_items: false,
        }
    }
    impl LcuApi for Fake {
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
                    ("GET", "/lol-summoner/v1/current-summoner") => Ok(json!({ "summonerId": 42 })),
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
    fn titles(v: &Value) -> Vec<String> {
        v.as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap_or("").to_string()).collect()
    }
    fn set_titles(v: &Value) -> Vec<String> {
        v.as_array().unwrap().iter().map(|b| b["title"].as_str().unwrap_or("").to_string()).collect()
    }
    fn names(f: &Fake) -> Vec<String> {
        f.pages.lock().iter().map(|p| p["name"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn rune_page_and_item_set_bodies() {
        let build = build_json(None);
        assert_eq!(rune_page_body(&build).unwrap()["selectedPerkIds"], json!([8010, 9111, 9104, 8299, 8444, 8242, 5008, 5008, 5001]));
        let set = item_set_body(&build, "Irelia Companion · Top vs Darius", "u1", None);
        assert_eq!(set["associatedChampions"], json!([39]));
        assert_eq!(titles(&set["blocks"]), vec!["Start", "Core and boots, in order", "4th item (best first)", "5th item (best first)", "Vision and potions"]);
        assert_eq!(set["blocks"][0]["items"], json!([{ "id": "1055", "count": 1 }, { "id": "2003", "count": 2 }]));
        let ids = |b: &Value| b["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(&set["blocks"][1]), vec!["3153", "3078", "6333", "3047"]);
        assert_eq!(set["blocks"][2]["items"].as_array().unwrap().len(), 3);
        let early = item_set_body(&build_json(Some(1.0)), "t", "u2", None);
        assert_eq!(ids(&early["blocks"][1]), vec!["3153", "3047", "3078", "6333"]);
    }

    #[test]
    fn versus_blocks() {
        let build = build_json(None);
        let best = json!({ "start": { "value": [1036, 2003], "games": 24, "lift": 0.04 }, "first": [{ "value": 3053, "games": 31, "lift": 0.05 }, { "value": 3153, "games": 40, "lift": 0.02 }] });
        let set = item_set_body(&build, "t", "u3", Some(&VersusBlocks { name: "Darius".into(), best: best.clone() }));
        assert_eq!(titles(&set["blocks"]), vec![
            "Start", "vs Darius: best start (24 games)", "Core and boots, in order", "vs Darius: best first item (31 · 40 games)",
            "4th item (best first)", "5th item (best first)", "Vision and potions",
        ]);
        assert_eq!(set["blocks"][1]["items"], json!([{ "id": "1036", "count": 1 }, { "id": "2003", "count": 1 }]));
        let only = item_set_body(&build, "t", "u4", Some(&VersusBlocks { name: "Darius".into(), best: json!({ "start": null, "first": [best["first"][0].clone()] }) }));
        assert_eq!(titles(&only["blocks"]).into_iter().filter(|t| t.starts_with("vs")).collect::<Vec<_>>(), vec!["vs Darius: best first item (31 games)"]);
    }

    fn opts(lane: &'static str, opponent: Option<&str>, rune: Option<i64>, set: Option<&str>) -> ImportOptions {
        ImportOptions { lane, opponent_name: opponent.map(String::from), rune_page_id: rune, item_set_uid: set.map(String::from), best: None }
    }

    #[tokio::test]
    async fn first_import_keeps_your_pages_and_sets() {
        let f = fake(vec![json!({ "id": 1, "name": "My Irelia" }), json!({ "id": 2, "name": "Default", "isEditable": false })], 5.0, vec![json!({ "uid": "mine", "title": "My set" })]);
        let r = import_build(&f, &build_json(None), &opts("TOP", Some("Darius"), None, None)).await;
        assert_eq!((r["runePage"]["ok"].clone(), r["runePage"]["replaced"].clone()), (json!(true), json!(false)));
        assert_eq!(r["itemSet"]["title"], "Irelia Companion · Top vs Darius");
        assert_eq!(names(&f), vec!["My Irelia", "Default", PAGE_NAME]);
        assert_eq!(set_titles(&f.sets.lock()["itemSets"]), vec!["My set", "Irelia Companion · Top vs Darius"]);
        assert_eq!(f.sets.lock()["accountId"], json!(7));
        assert_eq!(*f.writes.lock(), vec!["POST /lol-perks/v1/pages", "PUT /lol-item-sets/v1/item-sets/42/sets"]);
        assert_eq!(r["owned"]["runePageId"], f.pages.lock()[2]["id"]);
    }

    #[tokio::test]
    async fn importing_again_replaces_only_its_own() {
        let f = fake(
            vec![json!({ "id": 1, "name": "My Irelia" }), json!({ "id": 77, "name": PAGE_NAME })],
            5.0,
            vec![json!({ "uid": "mine", "title": "My set" }), json!({ "uid": "old", "title": "Irelia Companion · Mid vs Syndra" })],
        );
        let r = import_build(&f, &build_json(None), &opts("TOP", None, Some(77), Some("old"))).await;
        assert_eq!(r["runePage"]["replaced"], json!(true));
        assert_eq!(r["itemSet"]["replaced"], json!(true));
        assert_eq!(names(&f), vec!["My Irelia", PAGE_NAME]);
        assert_ne!(f.pages.lock()[1]["id"], json!(77));
        assert_eq!(set_titles(&f.sets.lock()["itemSets"]), vec!["My set", "Irelia Companion · Top"]);
        assert_eq!(*f.writes.lock(), vec!["DELETE /lol-perks/v1/pages/77", "POST /lol-perks/v1/pages", "PUT /lol-item-sets/v1/item-sets/42/sets"]);
    }

    #[tokio::test]
    async fn no_free_rune_page() {
        let f = fake(vec![json!({ "id": 1, "name": "A" }), json!({ "id": 2, "name": "B" })], 2.0, vec![]);
        let r = import_build(&f, &build_json(None), &opts("MIDDLE", Some("Syndra"), None, None)).await;
        assert_eq!(r["runePage"]["ok"], json!(false));
        assert!(r["runePage"]["error"].as_str().unwrap().contains("No free rune page (2 of 2 in use)"));
        assert_eq!(r["itemSet"]["ok"], json!(true));
        assert_eq!(names(&f), vec!["A", "B"]);
        assert!(!f.writes.lock().iter().any(|w| w.starts_with("DELETE")));
    }

    #[tokio::test]
    async fn a_client_error_on_one_part_leaves_the_other() {
        let mut f = fake(vec![], 5.0, vec![]);
        f.fail_items = true;
        let r = import_build(&f, &build_json(None), &opts("TOP", None, None, Some("keep"))).await;
        assert_eq!(r["runePage"]["ok"], json!(true));
        assert_eq!(r["itemSet"]["ok"], json!(false));
        assert_eq!(r["owned"]["itemSetUid"], json!("keep"));
    }
}
