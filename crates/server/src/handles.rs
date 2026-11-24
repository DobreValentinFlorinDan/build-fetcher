//! What the server needs from the store (Agent A, crates/store) and the learner (Agent B, crates/learn), as small
//! traits. The server is written and tested against these with in-memory fakes (`fakes.rs`); at integration the lead
//! implements them over `bf_store` and `bf_learn` and returns them from `wiring::default_handles`, the one place the
//! real parts are plugged in. Recommendations come through the contract's own `BuildSource` trait.
//!
//! Every method is called from the server's async tasks and must not block for long: the status methods are polled
//! every few seconds for the dashboard. Slow work (a training cycle, a cleanup) belongs on the implementer's threads.

use bf_core::build::{Activity, BuildSource, FetchStatus, ModelStatus, PairVolume, Platform, RankBand, StorageStatus};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The settings the dashboard shows and edits (a view over `bf_store`'s typed settings).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    /// Platforms games are fetched from (default EUN1, EUW1, KR).
    pub regions: Vec<Platform>,
    /// The lowest rank sampled: MasterPlus, or Diamond (MasterPlus widens to Diamond for thin pairs either way).
    pub rank_floor: RankBand,
    pub disk_budget_bytes: u64,
    /// Tier A champions the user added (the store adds detected ones on its own; they are in `tier_a_detected`).
    pub tier_a: Vec<i64>,
    pub tier_a_detected: Vec<i64>,
    pub gpu: bool,
    pub learning: bool,
    /// The first-run steps (key, region) were completed: fetching may start.
    pub first_run_complete: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            regions: Platform::ALL.to_vec(),
            rank_floor: RankBand::MasterPlus,
            disk_budget_bytes: 2 * 1024 * 1024 * 1024,
            tier_a: vec![],
            tier_a_detected: vec![],
            gpu: true,
            learning: true,
            first_run_complete: false,
        }
    }
}

/// A change from the dashboard: only the fields given change. Validated by the server (`routes::validate_patch`)
/// before the store sees it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regions: Option<Vec<Platform>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank_floor: Option<RankBand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_budget_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_a: Option<Vec<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_complete: Option<bool>,
}

impl SettingsPatch {
    pub fn apply(&self, s: &mut AppSettings) {
        if let Some(v) = &self.regions {
            s.regions = v.clone();
        }
        if let Some(v) = self.rank_floor {
            s.rank_floor = v;
        }
        if let Some(v) = self.disk_budget_bytes {
            s.disk_budget_bytes = v;
        }
        if let Some(v) = &self.tier_a {
            s.tier_a = v.clone();
        }
        if let Some(v) = self.gpu {
            s.gpu = v;
        }
        if let Some(v) = self.learning {
            s.learning = v;
        }
        if let Some(v) = self.first_run_complete {
            s.first_run_complete = v;
        }
    }
}

/// The signed-in League account, from the client: the user's own, the only player whose identity the app reads
/// (their own games are fetched first).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnAccount {
    pub puuid: String,
    pub game_name: Option<String>,
    pub tag_line: Option<String>,
    /// The client's platform id ("EUN1"), when it says.
    pub platform: Option<String>,
}

/// The store, as the server sees it.
pub trait StoreHandle: Send + Sync {
    fn fetch_status(&self) -> FetchStatus;
    fn storage_status(&self) -> StorageStatus;
    /// Games per champion-role and patch (`DataSource::volumes(&[])`): the dashboard's data age per champion.
    fn volumes(&self) -> Vec<PairVolume>;
    /// Fetching pauses outside Idle/Busy (the scheduler's `Activity` watch).
    fn set_activity(&self, activity: Activity);
    fn settings(&self) -> AppSettings;
    fn update_settings(&self, patch: &SettingsPatch) -> anyhow::Result<AppSettings>;
    /// The user's own account, once the League client says who is signed in.
    fn set_account(&self, account: &OwnAccount);
    /// The user's game just ended: their new game is fetched first.
    fn game_ended(&self);
}

/// The learner, as the server sees it (its recommendations come through `BuildSource`).
pub trait LearnerHandle: Send + Sync {
    fn status(&self) -> ModelStatus;
    /// Makes `version` the active checkpoint again.
    fn rollback(&self, version: &str) -> anyhow::Result<ModelStatus>;
    /// Heavy work runs only when Idle.
    fn set_activity(&self, activity: Activity);
}

/// Whether Irelia Companion's key can be offered ("Use the same key as Irelia Companion"). Never the key itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum KeyOffer {
    Available,
    Missing,
    Empty,
    /// The file exists but could not be read or parsed; the reason never holds the key.
    Unreadable { reason: String },
}

/// The Riot API key, as the server sees it. Nothing here returns the key: only `mask_key`'s form.
pub trait KeyHandle: Send + Sync {
    fn irelia_offer(&self) -> KeyOffer;
    /// On the user's click: copies Irelia Companion's `riotApiKey` into Build Fetcher's settings.
    fn import_irelia_key(&self) -> anyhow::Result<()>;
    /// A pasted key (already trimmed and shape-checked by the server).
    fn set_key(&self, key: &str) -> anyhow::Result<()>;
    /// `bf_core::config::mask_key` of the stored key, "" when none is set.
    fn key_masked(&self) -> String;
    /// Riot's last refusal of the key ("401 Unauthorized", "403 Forbidden"), until a new key is set.
    fn key_error(&self) -> Option<String>;
}

/// Everything the server is given.
#[derive(Clone)]
pub struct Handles {
    pub builds: Arc<dyn BuildSource>,
    pub store: Arc<dyn StoreHandle>,
    pub learner: Arc<dyn LearnerHandle>,
    pub keys: Arc<dyn KeyHandle>,
}
