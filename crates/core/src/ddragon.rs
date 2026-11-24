//! Port of `server/ddragon.ts`: Data Dragon's static data (items, champions, runes, summoners, Irelia's spells),
//! cached per version under `<data>/ddragon/<version>/`, and the fingerprints patch memory compares.

use crate::config::{config, CHAMPION_KEY};
use crate::js;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

const CDN: &str = "https://ddragon.leagueoflegends.com";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemMeta {
    pub id: i64,
    pub name: String,
    pub tags: Vec<String>,
    pub into: Vec<i64>,
    pub from: Vec<i64>,
    pub special_recipe: Option<i64>,
    pub depth: i64,
    pub gold_total: f64,
    pub purchasable: bool,
    pub on_summoners_rift: bool,
    pub in_store: bool,
    pub passives: Vec<String>,
    pub armor: f64,
    pub magic_resist: f64,
    pub anti_heal: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChampionInfo {
    pub attack: f64,
    pub defense: f64,
    pub magic: f64,
    pub difficulty: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChampionMeta {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub tags: Vec<String>,
    pub info: ChampionInfo,
    pub attack_range: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rune {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub icon: String,
    #[serde(default)]
    pub short_desc: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuneSlot {
    pub runes: Vec<Rune>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuneTree {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub icon: String,
    pub slots: Vec<RuneSlot>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Summoner {
    pub name: String,
    pub image: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Spell {
    pub key: String,
    pub name: String,
    pub image: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fingerprints {
    pub items: HashMap<i64, String>,
    pub champion: String,
}

#[derive(Clone, Debug)]
pub struct StaticData {
    pub version: String,
    pub patch: String,
    pub previous_patch: String,
    /// By id, ascending (JS Map built from Object.entries of integer keys).
    pub items: BTreeMap<i64, ItemMeta>,
    /// In champion.json's order (Object.values of non-index keys: insertion order).
    pub champions: indexmap::IndexMap<i64, ChampionMeta>,
    pub champions_by_key: HashMap<String, i64>,
    pub runes: Vec<RuneTree>,
    pub rune_ids: HashSet<i64>,
    pub summoners: indexmap::IndexMap<i64, Summoner>,
    pub spells: Vec<Spell>,
    pub fingerprints: Fingerprints,
    pub previous: Option<(String, Fingerprints)>,
    pub history: Option<HashMap<String, Fingerprints>>,
    pub versions: Option<Vec<String>>,
}

pub type Static = Arc<StaticData>;

impl StaticData {
    pub fn item(&self, id: i64) -> Option<&ItemMeta> {
        self.items.get(&id)
    }
    pub fn champion(&self, id: i64) -> Option<&ChampionMeta> {
        self.champions.get(&id)
    }
    pub fn champion_by_key(&self, key: &str) -> Option<&ChampionMeta> {
        self.champions_by_key.get(key).and_then(|id| self.champions.get(id))
    }
}

/// "16.19.1" or "16.19.712.4471" -> "16.19"
pub fn to_patch(version: &str) -> String {
    let mut it = version.split('.');
    let major = it.next().unwrap_or("undefined");
    let minor = it.next().unwrap_or("undefined");
    format!("{major}.{minor}")
}

fn patch_parts(p: &str) -> (f64, f64) {
    let mut it = p.split('.').map(|x| if x.is_empty() { 0.0 } else { x.parse::<f64>().unwrap_or(f64::NAN) });
    (it.next().unwrap_or(f64::NAN), it.next().unwrap_or(f64::NAN))
}

/// `a1 - b1 || a2 - b2` (as a number: NaN for unparseable patches, which compares false both ways).
pub fn compare_patch_num(a: &str, b: &str) -> f64 {
    let (a1, a2) = patch_parts(a);
    let (b1, b2) = patch_parts(b);
    let d = a1 - b1;
    if d != 0.0 && !d.is_nan() {
        d
    } else {
        a2 - b2
    }
}

/// The sign of compare_patch_num as an i32-like number for `> 0` / `< 0` tests (NaN → 0 but both tests false).
pub fn compare_patch(a: &str, b: &str) -> f64 {
    compare_patch_num(a, b)
}

/// A sort comparator from compare_patch (NaN treated as equal, as Array.prototype.sort does).
pub fn patch_ordering(a: &str, b: &str) -> std::cmp::Ordering {
    let d = compare_patch_num(a, b);
    if d < 0.0 {
        std::cmp::Ordering::Less
    } else if d > 0.0 {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?)
}

async fn fetch_json(url: &str) -> Result<Value> {
    let res = http()?.get(url).send().await?;
    if !res.status().is_success() {
        return Err(anyhow!("Data Dragon {} for {}", res.status().as_u16(), url));
    }
    Ok(res.json().await?)
}

async fn cached(dir: &Path, version: &str, file: &str, url: &str) -> Result<Value> {
    let target = dir.join(version).join(file);
    if let Ok(t) = std::fs::read_to_string(&target) {
        if let Ok(v) = serde_json::from_str(&t) {
            return Ok(v);
        }
    }
    let data = fetch_json(url).await?;
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&target, js::stringify(&data))?;
    Ok(data)
}

/// Items and Irelia as comparable strings: gold, stats and text for items; stats, spell values and texts for Irelia.
pub fn fingerprints(item: &Value, champ: &Value) -> Fingerprints {
    let mut items = HashMap::new();
    if let Some(data) = item.get("data").and_then(|d| d.as_object()) {
        for (id, it) in js::entries(data) {
            let fp = json!([
                it.get("gold").and_then(|g| g.get("total")).cloned().unwrap_or(Value::Null),
                it.get("stats").cloned().unwrap_or_else(|| json!({})),
                it.get("description").cloned().unwrap_or(Value::Null)
            ]);
            items.insert(id.parse::<i64>().unwrap_or(0), js::stringify(&fp));
        }
    }
    let c = champ.get("data").and_then(|d| d.get(CHAMPION_KEY));
    let undef = |v: Option<&Value>| v.cloned().unwrap_or(Value::Null);
    let spells: Vec<Value> = c
        .and_then(|c| c.get("spells"))
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .map(|s| {
                    json!([
                        undef(s.get("tooltip")),
                        undef(s.get("cooldown")),
                        undef(s.get("cost")),
                        undef(s.get("effect")),
                        undef(s.get("datavalues")),
                        undef(s.get("range"))
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    let mut m = Map::new();
    m.insert("stats".into(), c.and_then(|c| c.get("stats")).cloned().unwrap_or_else(|| json!({})));
    m.insert("spells".into(), Value::Array(spells));
    m.insert(
        "passive".into(),
        c.and_then(|c| c.get("passive")).and_then(|p| p.get("description")).cloned().unwrap_or_else(|| json!("")),
    );
    Fingerprints { items, champion: js::stringify(&Value::Object(m)) }
}

/// The newest version of each of the `count` newest patches in `versions` (newest first), by patch.
pub fn history_versions(versions: &[String], count: usize) -> Vec<(String, String)> {
    let mut newest: Vec<(String, String)> = Vec::new();
    for v in versions {
        let p = to_patch(v);
        if !newest.iter().any(|(q, _)| *q == p) {
            newest.push((p, v.clone()));
        }
        if newest.len() >= count {
            break;
        }
    }
    newest
}

/// Fingerprints of the newest version of each of the `count` newest patches (cached on disk, fetched once). A patch
/// whose files can't be had is left out: patch memory then treats it as unknown.
pub async fn load_patch_history(versions: &[String], count: usize) -> HashMap<String, Fingerprints> {
    let dir = config().ddragon_cache_dir;
    let mut out = HashMap::new();
    for (patch, version) in history_versions(versions, count) {
        let base = format!("{CDN}/cdn/{version}/data/en_US");
        let item = cached(&dir, &version, "item.json", &format!("{base}/item.json")).await;
        let champ = cached(&dir, &version, &format!("{CHAMPION_KEY}.json"), &format!("{base}/champion/{CHAMPION_KEY}.json")).await;
        if let (Ok(item), Ok(champ)) = (item, champ) {
            out.insert(patch, fingerprints(&item, &champ));
        }
    }
    out
}

/// The newest Data Dragon version and its files (cached on disk). `known` skips asking for the version list again.
pub async fn load_static_data(known: Option<&[String]>) -> Result<StaticData> {
    let dir = config().ddragon_cache_dir;
    let versions: Vec<String> = match known.filter(|k| !k.is_empty()) {
        Some(k) => k.to_vec(),
        None => match fetch_json(&format!("{CDN}/api/versions.json")).await {
            Ok(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => {
                let mut dirs: Vec<String> = std::fs::read_dir(&dir)
                    .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                    .unwrap_or_default();
                if dirs.is_empty() {
                    return Err(anyhow!("Data Dragon unreachable and no cache present"));
                }
                dirs.sort_by(|a, b| patch_ordering(b, a));
                dirs
            }
        },
    };
    let version = versions[0].clone();
    let patch = to_patch(&version);
    let previous_patch = versions.iter().map(|v| to_patch(v)).find(|p| *p != patch).unwrap_or_else(|| patch.clone());
    let previous_version = versions.iter().find(|v| to_patch(v) == previous_patch && previous_patch != patch).cloned();
    let base = format!("{CDN}/cdn/{version}/data/en_US");
    let raw = RawStaticData {
        item: cached(&dir, &version, "item.json", &format!("{base}/item.json")).await?,
        champion: cached(&dir, &version, "champion.json", &format!("{base}/champion.json")).await?,
        runes_reforged: cached(&dir, &version, "runesReforged.json", &format!("{base}/runesReforged.json")).await?,
        summoner: cached(&dir, &version, "summoner.json", &format!("{base}/summoner.json")).await?,
        champ: cached(&dir, &version, &format!("{CHAMPION_KEY}.json"), &format!("{base}/champion/{CHAMPION_KEY}.json")).await?,
    };
    let mut previous = None;
    if let Some(pv) = previous_version {
        let pb = format!("{CDN}/cdn/{pv}/data/en_US");
        let item = cached(&dir, &pv, "item.json", &format!("{pb}/item.json")).await;
        let champ = cached(&dir, &pv, &format!("{CHAMPION_KEY}.json"), &format!("{pb}/champion/{CHAMPION_KEY}.json")).await;
        if let (Ok(item), Ok(champ)) = (item, champ) {
            previous = Some((pv, item, champ));
        }
    }
    let mut data = build_static_data(&version, &previous_patch, &raw, previous.as_ref().map(|(v, i, c)| (v.as_str(), i, c)))?;
    data.versions = Some(versions.into_iter().take(40).collect());
    Ok(data)
}

/// The Data Dragon files the app reads for one version.
pub struct RawStaticData {
    pub item: Value,
    pub champion: Value,
    pub runes_reforged: Value,
    pub summoner: Value,
    pub champ: Value,
}

fn passives(desc: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = desc;
    while let Some(i) = rest.find("<passive>") {
        let after = &rest[i + 9..];
        // [^<]+ then </passive>
        let end = after.find('<').unwrap_or(after.len());
        if end > 0 && after[end..].starts_with("</passive>") {
            let name = after[..end].trim().to_string();
            if !out.contains(&name) {
                out.push(name);
            }
            rest = &after[end + 10..];
        } else {
            rest = after;
        }
    }
    out
}

fn anti_heal(desc: &str) -> bool {
    // /<passive>\s*Grievous Wounds\s*<\/passive>/
    let mut rest = desc;
    while let Some(i) = rest.find("<passive>") {
        let after = &rest[i + 9..];
        let t = after.trim_start();
        if let Some(t2) = t.strip_prefix("Grievous Wounds") {
            if t2.trim_start().starts_with("</passive>") {
                return true;
            }
        }
        rest = after;
    }
    false
}

/// StaticData from raw Data Dragon files: the live loader above, and the tests' committed snapshot.
pub fn build_static_data(
    version: &str,
    previous_patch: &str,
    raw: &RawStaticData,
    previous: Option<(&str, &Value, &Value)>,
) -> Result<StaticData> {
    let patch = to_patch(version);
    let mut items = BTreeMap::new();
    let item_data = raw.item.get("data").and_then(|d| d.as_object()).ok_or_else(|| anyhow!("item.json has no data"))?;
    for (id, it) in js::entries(item_data) {
        let nid: i64 = id.parse().unwrap_or(0);
        let strs = |k: &str| -> Vec<String> {
            it.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
        };
        let nums = |k: &str| -> Vec<i64> {
            it.get(k).and_then(|v| v.as_array()).map(|a| a.iter().map(|x| js::to_number(x) as i64).collect()).unwrap_or_default()
        };
        let desc = it.get("description").and_then(|d| d.as_str()).unwrap_or("");
        let stat = |k: &str| it.get("stats").and_then(|s| s.get(k)).and_then(|v| v.as_f64()).unwrap_or(0.0);
        items.insert(
            nid,
            ItemMeta {
                id: nid,
                name: it.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                tags: strs("tags"),
                into: nums("into"),
                from: nums("from"),
                special_recipe: it.get("specialRecipe").filter(|v| js::truthy(v)).map(|v| js::to_number(v) as i64),
                depth: it.get("depth").and_then(|v| v.as_i64()).unwrap_or(1),
                gold_total: it.get("gold").and_then(|g| g.get("total")).and_then(|v| v.as_f64()).unwrap_or(0.0),
                purchasable: it.get("gold").and_then(|g| g.get("purchasable")).and_then(|v| v.as_bool()).unwrap_or(false),
                on_summoners_rift: it.get("maps").and_then(|m| m.get("11")).and_then(|v| v.as_bool()) == Some(true),
                in_store: it.get("inStore").and_then(|v| v.as_bool()) != Some(false),
                passives: passives(desc),
                armor: stat("FlatArmorMod"),
                magic_resist: stat("FlatSpellBlockMod"),
                anti_heal: anti_heal(desc),
            },
        );
    }

    let mut champions = indexmap::IndexMap::new();
    let mut champions_by_key = HashMap::new();
    if let Some(data) = raw.champion.get("data").and_then(|d| d.as_object()) {
        for (_, c) in js::entries(data) {
            let id = js::to_number(c.get("key").unwrap_or(&Value::Null)) as i64;
            let meta = ChampionMeta {
                id,
                key: c.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: c.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                tags: c
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                info: c.get("info").and_then(|i| serde_json::from_value(i.clone()).ok()).unwrap_or(ChampionInfo {
                    attack: 0.0,
                    defense: 0.0,
                    magic: 0.0,
                    difficulty: 0.0,
                }),
                attack_range: c.get("stats").and_then(|s| s.get("attackrange")).and_then(|v| v.as_f64()).unwrap_or(0.0),
            };
            champions_by_key.insert(meta.key.clone(), id);
            champions_by_key.insert(meta.name.clone(), id);
            champions.insert(id, meta);
        }
    }

    let runes: Vec<RuneTree> = serde_json::from_value(raw.runes_reforged.clone())?;
    let mut rune_ids = HashSet::new();
    for tree in &runes {
        rune_ids.insert(tree.id);
        for slot in &tree.slots {
            for r in &slot.runes {
                rune_ids.insert(r.id);
            }
        }
    }

    let mut summoners = indexmap::IndexMap::new();
    if let Some(data) = raw.summoner.get("data").and_then(|d| d.as_object()) {
        for (_, s) in js::entries(data) {
            summoners.insert(
                js::to_number(s.get("key").unwrap_or(&Value::Null)) as i64,
                Summoner {
                    name: s.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                    image: s.get("image").and_then(|i| i.get("full")).and_then(|v| v.as_str()).unwrap_or("").into(),
                },
            );
        }
    }

    let keys = ["Q", "W", "E", "R"];
    let spells: Vec<Spell> = raw
        .champ
        .get("data")
        .and_then(|d| d.get(CHAMPION_KEY))
        .and_then(|c| c.get("spells"))
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow!("{CHAMPION_KEY}.json has no spells"))?
        .iter()
        .enumerate()
        .map(|(i, s)| Spell {
            key: keys.get(i).copied().unwrap_or("undefined").into(),
            name: s.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
            image: s.get("image").and_then(|i| i.get("full")).and_then(|v| v.as_str()).unwrap_or("").into(),
        })
        .collect();

    Ok(StaticData {
        version: version.into(),
        patch,
        previous_patch: previous_patch.into(),
        items,
        champions,
        champions_by_key,
        runes,
        rune_ids,
        summoners,
        spells,
        fingerprints: fingerprints(&raw.item, &raw.champ),
        previous: previous.map(|(v, i, c)| (v.to_string(), fingerprints(i, c))),
        history: None,
        versions: None,
    })
}
