//! Data Dragon for the server: loaded in the background at start (cached on disk by `bf_core::ddragon`), retried
//! until it arrives, so the server answers at once even offline. Champion names, item names and icons for the
//! dashboard, champion ids for the Live Client (which names champions, not ids).

use bf_core::ddragon::StaticData;
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Data Dragon once loaded (None until then).
pub type StaticSlot = Arc<RwLock<Option<Arc<StaticData>>>>;

pub fn empty_slot() -> StaticSlot {
    Arc::new(RwLock::new(None))
}

/// Loads Data Dragon into `slot`, retrying every minute until it succeeds; `ready` runs once it is in.
pub fn load_in_background(slot: StaticSlot, ready: impl Fn(&StaticData) + Send + Sync + 'static) {
    tokio::spawn(async move {
        loop {
            match bf_core::ddragon::load_static_data(None).await {
                Ok(d) => {
                    crate::bf_log!("[ddragon] {} ({} champions, {} items)", d.version, d.champions.len(), d.items.len());
                    let d = Arc::new(d);
                    *slot.write() = Some(d.clone());
                    ready(&d);
                    return;
                }
                Err(e) => crate::bf_log!("[ddragon] not loaded yet ({e}); trying again in a minute"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChampionView {
    pub id: i64,
    /// Data Dragon's id ("MonkeyKing"): the icon's file name.
    pub key: String,
    pub name: String,
    pub tags: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedIcon {
    pub name: String,
    /// The icon's path or file, as Data Dragon names it (the dashboard builds the CDN URL).
    pub icon: String,
}

/// GET /api/static: what the dashboard needs to label and draw options.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticView {
    pub ready: bool,
    pub version: Option<String>,
    pub patch: Option<String>,
    pub champions: Vec<ChampionView>,
    pub items: BTreeMap<i64, NamedIcon>,
    pub runes: BTreeMap<i64, NamedIcon>,
    pub summoners: BTreeMap<i64, NamedIcon>,
}

pub fn view(d: Option<&StaticData>) -> StaticView {
    let Some(d) = d else {
        return StaticView { ready: false, version: None, patch: None, champions: vec![], items: BTreeMap::new(), runes: BTreeMap::new(), summoners: BTreeMap::new() };
    };
    let mut champions: Vec<ChampionView> =
        d.champions.values().map(|c| ChampionView { id: c.id, key: c.key.clone(), name: c.name.clone(), tags: c.tags.clone() }).collect();
    champions.sort_by(|a, b| a.name.cmp(&b.name));
    let items = d
        .items
        .values()
        .filter(|i| i.on_summoners_rift && i.id < 100_000)
        .map(|i| (i.id, NamedIcon { name: i.name.clone(), icon: format!("{}.png", i.id) }))
        .collect();
    let mut runes = BTreeMap::new();
    for t in &d.runes {
        runes.insert(t.id, NamedIcon { name: t.name.clone(), icon: t.icon.clone() });
        for s in &t.slots {
            for r in &s.runes {
                runes.insert(r.id, NamedIcon { name: r.name.clone(), icon: r.icon.clone() });
            }
        }
    }
    let summoners = d.summoners.iter().map(|(id, s)| (*id, NamedIcon { name: s.name.clone(), icon: s.image.clone() })).collect();
    StaticView { ready: true, version: Some(d.version.clone()), patch: Some(d.patch.clone()), champions, items, runes, summoners }
}
