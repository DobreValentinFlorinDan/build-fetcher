//! The real store and key handles (`handles.rs` traits over `bf_store`).
//!
//! The store needs Data Dragon to parse matches (item classes, champion tags), which the server loads in the
//! background. So `RealStore` starts empty, opens the database once Data Dragon arrives, and starts fetching once the
//! first run is complete and a key is set. Until then the status methods answer with what is known. Nothing here ever
//! returns or logs the key: only `mask_key`'s form leaves this file.

use crate::handles::{AppSettings, KeyHandle, KeyOffer, OwnAccount, SettingsPatch, StoreHandle};
use crate::static_data::StaticSlot;
use bf_core::build::{Activity, DataSource, FetchStatus, PairVolume, Platform, StorageStatus};
use bf_core::riot::client::RiotClient;
use bf_store::{Fetcher, FetcherConfig, OtherParts, Settings, Store, StoreOptions};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::watch;

/// The dashboard's "first run done" flag; kept in settings.json next to the store's own keys (the store keeps keys it
/// doesn't know).
const FIRST_RUN_FIELD: &str = "firstRunComplete";

pub struct RealStore {
    settings_file: PathBuf,
    data_dir: PathBuf,
    logs_dir: PathBuf,
    store: OnceLock<Arc<Store>>,
    client: Arc<RiotClient>,
    fetcher: Mutex<Option<Fetcher<RiotClient>>>,
    activity: watch::Sender<Activity>,
    /// Called with the store once it is open (the learner is attached to it there).
    on_open: Mutex<Vec<Box<dyn FnOnce(Arc<Store>) + Send>>>,
}

impl RealStore {
    /// `settings_file` and `data_dir` are explicit so tests can point them at a temporary folder.
    pub fn new(settings_file: PathBuf, data_dir: PathBuf, logs_dir: PathBuf) -> Arc<RealStore> {
        let cfg = bf_core::config::config();
        let key = bf_store::settings::api_key(&settings_file).unwrap_or_default();
        let client = Arc::new(RiotClient::new(&key, cfg.rate_limits.clone(), cfg.pacing));
        let (activity, _) = watch::channel(Activity::Idle);
        Arc::new(RealStore {
            settings_file,
            data_dir,
            logs_dir,
            store: OnceLock::new(),
            client,
            fetcher: Mutex::new(None),
            activity,
            on_open: Mutex::new(vec![]),
        })
    }

    /// Opens the database once Data Dragon is loaded, then starts fetching if it may.
    pub fn open_when_ready(self: &Arc<Self>, slot: StaticSlot) {
        let me = self.clone();
        tokio::spawn(async move {
            let data = loop {
                if let Some(d) = slot.read().clone() {
                    break d;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            };
            let dir = me.data_dir.clone();
            let opened = tokio::task::spawn_blocking(move || Store::open(&dir, data, StoreOptions::default())).await;
            match opened {
                Ok(Ok(store)) => {
                    let store = Arc::new(store);
                    let _ = me.store.set(store.clone());
                    crate::bf_log!("[store] opened {}", me.data_dir.display());
                    for f in me.on_open.lock().drain(..) {
                        f(store.clone());
                    }
                    me.maybe_start();
                }
                Ok(Err(e)) => crate::bf_log!("[store] could not open the database: {}", crate::routes::scrub(&e.to_string())),
                Err(e) => crate::bf_log!("[store] could not open the database: {e}"),
            }
        });
    }

    /// Runs `f` with the store as soon as it is open (at once if it already is).
    pub fn with_store(&self, f: impl FnOnce(Arc<Store>) + Send + 'static) {
        match self.store.get() {
            Some(s) => f(s.clone()),
            None => self.on_open.lock().push(Box::new(f)),
        }
    }

    pub fn store(&self) -> Option<Arc<Store>> {
        self.store.get().cloned()
    }

    pub fn other_parts(&self) -> OtherParts {
        OtherParts::standard(&self.data_dir, Some(&self.logs_dir))
    }

    fn load_settings(&self) -> Settings {
        let (s, problems) = bf_store::settings::load(&self.settings_file);
        for p in problems {
            crate::bf_log!("[settings] {p}");
        }
        s
    }

    fn first_run_complete(&self) -> bool {
        bf_core::app_config::read_config_at(&self.settings_file).get(FIRST_RUN_FIELD).and_then(|v| v.as_bool()).unwrap_or(false)
    }

    fn key_present(&self) -> bool {
        bf_store::settings::api_key(&self.settings_file).is_some()
    }

    /// Starts the fetcher when the store is open, the first run is done and a key is set (once).
    fn maybe_start(&self) {
        let Some(store) = self.store() else { return };
        if !self.first_run_complete() || !self.key_present() {
            return;
        }
        let mut f = self.fetcher.lock();
        if f.is_some() {
            return;
        }
        let fetcher = Fetcher::new(self.client.clone(), store, self.load_settings(), FetcherConfig::default());
        fetcher.set_key_present(true);
        fetcher.spawn(self.activity.subscribe());
        crate::bf_log!("[fetch] started");
        *f = Some(fetcher);
    }

    /// A new key was stored: the client uses it at once, and fetching starts or resumes.
    fn key_changed(&self) {
        let key = bf_store::settings::api_key(&self.settings_file).unwrap_or_default();
        self.client.set_api_key(&key);
        if let Some(f) = self.fetcher.lock().as_ref() {
            f.set_key_present(!key.is_empty());
        }
        self.maybe_start();
    }

    fn view(&self, s: &Settings) -> AppSettings {
        let detected = self
            .fetcher
            .lock()
            .as_ref()
            .and_then(|f| f.tier_a().ok())
            .map(|set| {
                let mut v: Vec<i64> = set.into_iter().filter(|c| !s.tier_a.contains(c)).collect();
                v.sort_unstable();
                v
            })
            .unwrap_or_default();
        AppSettings {
            regions: s.regions.clone(),
            rank_floor: s.rank_floor,
            disk_budget_bytes: s.disk_budget_bytes,
            tier_a: s.tier_a.clone(),
            tier_a_detected: detected,
            gpu: s.gpu,
            learning: s.learning,
            fetching: s.fetching,
            timeline_sample: s.timeline_sample,
            first_run_complete: self.first_run_complete(),
        }
    }

    fn save(&self, s: &Settings) -> anyhow::Result<()> {
        bf_store::settings::save(&self.settings_file, s)?;
        if let Some(f) = self.fetcher.lock().as_ref() {
            f.update_settings(s.clone());
        }
        Ok(())
    }
}

impl StoreHandle for RealStore {
    fn fetch_status(&self) -> FetchStatus {
        if let Some(f) = self.fetcher.lock().as_ref() {
            return f.status();
        }
        let paused = if !self.load_settings().fetching {
            "paused by you"
        } else if self.store().is_none() {
            "opening the database"
        } else if !self.key_present() {
            "no API key"
        } else {
            "first run not finished"
        };
        FetchStatus { paused: Some(paused.into()), ..FetchStatus::default() }
    }

    fn storage_status(&self) -> StorageStatus {
        let budget = self.load_settings().disk_budget_bytes;
        match self.store() {
            Some(s) => s.storage_status(budget, &self.other_parts()).unwrap_or_else(|e| {
                crate::bf_log!("[storage] {}", crate::routes::scrub(&e.to_string()));
                StorageStatus { budget_bytes: budget, ..StorageStatus::default() }
            }),
            None => StorageStatus { budget_bytes: budget, ..StorageStatus::default() },
        }
    }

    fn volumes(&self) -> Vec<PairVolume> {
        self.store().and_then(|s| s.volumes(&[]).ok()).unwrap_or_default()
    }

    fn set_activity(&self, activity: Activity) {
        self.activity.send_replace(activity);
    }

    fn settings(&self) -> AppSettings {
        self.view(&self.load_settings())
    }

    fn update_settings(&self, patch: &SettingsPatch) -> anyhow::Result<AppSettings> {
        let mut s = self.load_settings();
        if let Some(v) = &patch.regions {
            s.regions = v.clone();
        }
        if let Some(v) = patch.rank_floor {
            s.rank_floor = v;
        }
        if let Some(v) = patch.disk_budget_bytes {
            s.disk_budget_bytes = v;
        }
        if let Some(v) = &patch.tier_a {
            s.tier_a = v.clone();
        }
        if let Some(v) = patch.gpu {
            s.gpu = v;
        }
        if let Some(v) = patch.learning {
            s.learning = v;
        }
        if let Some(v) = patch.fetching {
            s.fetching = v;
            crate::bf_log!("[fetch] {}", if v { "resumed by you" } else { "paused by you" });
        }
        if let Some(v) = patch.timeline_sample {
            s.timeline_sample = v;
            crate::bf_log!("[fetch] timelines for other champions: {:.0}%", v * 100.0);
        }
        self.save(&s)?;
        if let Some(done) = patch.first_run_complete {
            bf_core::app_config::update_config_at(&self.settings_file, vec![(FIRST_RUN_FIELD, Some(serde_json::Value::Bool(done)))])
                .map_err(|(_, e)| anyhow::anyhow!("could not save the settings: {e}"))?;
            self.maybe_start();
        }
        Ok(self.view(&s))
    }

    fn set_account(&self, account: &OwnAccount) {
        let mut s = self.load_settings();
        let riot_id = match (&account.game_name, &account.tag_line) {
            (Some(n), Some(t)) => Some(format!("{n}#{t}")),
            _ => s.riot_id.clone(),
        };
        let platform = account.platform.as_deref().and_then(Platform::parse).unwrap_or(s.my_platform);
        if s.puuid.as_deref() == Some(account.puuid.as_str()) && s.riot_id == riot_id && s.my_platform == platform {
            return;
        }
        s.puuid = Some(account.puuid.clone());
        s.riot_id = riot_id;
        s.my_platform = platform;
        if let Err(e) = self.save(&s) {
            crate::bf_log!("[settings] could not save the account: {}", crate::routes::scrub(&e.to_string()));
        }
    }

    fn game_ended(&self) {
        // The fetcher sees the InGame → Idle change on its activity watch and fetches the user's new game first.
    }

    fn export_backup(&self, path: &Path, include_key: bool) -> anyhow::Result<bf_store::backup::ExportReport> {
        let store = self.store().ok_or_else(|| anyhow::anyhow!("The database is still opening; try again in a moment."))?;
        let r = bf_store::backup::export(&store, &self.settings_file, &self.data_dir.join("checkpoints"), path, include_key)?;
        crate::bf_log!("[backup] exported {} ({:.1} MB, {} files, key {}) in {} ms", r.path.display(), r.bytes as f64 / 1e6, r.files, if r.includes_key { "included" } else { "left out" }, r.millis);
        Ok(r)
    }

    fn stage_import(&self, path: &Path) -> anyhow::Result<bf_store::backup::ImportSummary> {
        let r = bf_store::backup::stage_import(path, &self.config_dir())?;
        crate::bf_log!("[backup] import staged from {} (made by {}, {:.1} MB database); it applies at the next start", path.display(), r.version, r.database_bytes as f64 / 1e6);
        Ok(r)
    }

    fn pending_import(&self) -> Option<bf_store::backup::ImportSummary> {
        bf_store::backup::pending_import(&self.config_dir())
    }

    fn cancel_import(&self) -> anyhow::Result<()> {
        bf_store::backup::cancel_import(&self.config_dir())?;
        crate::bf_log!("[backup] staged import cancelled");
        Ok(())
    }
}

impl RealStore {
    /// The settings folder (settings.json, logs, a staged import).
    fn config_dir(&self) -> PathBuf {
        self.settings_file.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
    }
}

/// The key, over `bf_store`'s settings and the Irelia Companion import.
pub struct RealKeys {
    settings_file: PathBuf,
    irelia_dir: PathBuf,
    store: Arc<RealStore>,
}

impl RealKeys {
    pub fn new(settings_file: PathBuf, irelia_dir: PathBuf, store: Arc<RealStore>) -> RealKeys {
        RealKeys { settings_file, irelia_dir, store }
    }
}

fn offer_of(o: bf_store::KeyOffer) -> KeyOffer {
    match o {
        bf_store::KeyOffer::Available => KeyOffer::Available,
        bf_store::KeyOffer::Missing => KeyOffer::Missing,
        bf_store::KeyOffer::Empty => KeyOffer::Empty,
        bf_store::KeyOffer::Unreadable(reason) => KeyOffer::Unreadable { reason: crate::routes::scrub(&reason) },
    }
}

impl KeyHandle for RealKeys {
    fn irelia_offer(&self) -> KeyOffer {
        offer_of(bf_store::key_import::offer_at(&self.irelia_dir))
    }

    fn import_irelia_key(&self) -> anyhow::Result<()> {
        bf_store::key_import::import_from(&self.irelia_dir, &self.settings_file).map_err(|e| anyhow::anyhow!(crate::routes::scrub(&e.to_string())))?;
        self.store.key_changed();
        Ok(())
    }

    fn set_key(&self, key: &str) -> anyhow::Result<()> {
        bf_store::settings::set_key_at(&self.settings_file, key).map_err(|e| anyhow::anyhow!(crate::routes::scrub(&e.to_string())))?;
        self.store.key_changed();
        Ok(())
    }

    fn key_masked(&self) -> String {
        bf_store::settings::key_status(&self.settings_file).masked
    }

    fn key_error(&self) -> Option<String> {
        self.store.fetch_status().key_error
    }
}

/// The logs folder next to the settings file (bf-desktop writes its log there too).
pub fn logs_dir(settings_file: &Path) -> PathBuf {
    settings_file.parent().map(|p| p.join("logs")).unwrap_or_else(|| PathBuf::from("logs"))
}
