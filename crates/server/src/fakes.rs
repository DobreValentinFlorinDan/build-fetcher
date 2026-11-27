//! In-memory stand-ins for the store, the learner and the key until they are wired in (`wiring.rs`), and for the
//! tests. The stub `BuildSource` gives plausible, clearly labelled sample builds for any champion so the dashboard and
//! the overlay can be used end to end; every recommendation it makes carries a note saying it is sample data.

use crate::handles::{AppSettings, Handles, KeyHandle, KeyOffer, LearnerHandle, OwnAccount, SettingsPatch, StoreHandle};
use crate::static_data::StaticSlot;
use bf_core::build::*;
use bf_core::config::mask_key;
use parking_lot::Mutex;
use std::sync::Arc;

pub const STUB_NOTE: &str = "Sample data: the learner is not connected yet, so these numbers are made up.";

/// All four fakes, sharing nothing but the Data Dragon slot (for champion classes in the stub).
pub fn handles(data: StaticSlot) -> Handles {
    Handles {
        builds: Arc::new(StubBuildSource { data }),
        store: Arc::new(FakeStore::default()),
        learner: Arc::new(FakeLearner::default()),
        keys: Arc::new(FakeKeys::new(KeyOffer::Missing)),
    }
}

// ------------------------------------------------------------------------------------------------ store

pub struct FakeStore {
    pub settings: Mutex<AppSettings>,
    pub activity: Mutex<Vec<Activity>>,
    pub account: Mutex<Option<OwnAccount>>,
    pub games_ended: Mutex<u32>,
    pub pending: Mutex<Option<bf_store::backup::ImportSummary>>,
}

impl Default for FakeStore {
    fn default() -> Self {
        FakeStore { settings: Mutex::new(AppSettings::default()), activity: Mutex::new(vec![]), account: Mutex::new(None), games_ended: Mutex::new(0), pending: Mutex::new(None) }
    }
}

impl StoreHandle for FakeStore {
    fn fetch_status(&self) -> FetchStatus {
        let paused = match self.activity.lock().last() {
            Some(Activity::ChampSelect) => Some("champ select".to_string()),
            Some(Activity::InGame) => Some("in game".to_string()),
            _ if !self.settings.lock().first_run_complete => Some("no API key".to_string()),
            _ => None,
        };
        FetchStatus {
            running: paused.is_none(),
            paused,
            budgets: vec![
                BudgetStatus { scope: "EUROPE".into(), limits: "20:1,100:120".into(), used: 61, ours: 48, limit: 100, window_s: 120, shared: true },
                BudgetStatus { scope: "ASIA".into(), limits: "20:1,100:120".into(), used: 22, ours: 22, limit: 100, window_s: 120, shared: false },
                BudgetStatus { scope: "eun1".into(), limits: "20:1,100:120".into(), used: 4, ours: 4, limit: 100, window_s: 120, shared: false },
            ],
            requests_last_hour: 2410,
            matches_last_hour: 312,
            matches_total: 18_450,
            dropped: vec![("remake".into(), 41), ("leaver".into(), 17), ("roles".into(), 9), ("duplicate".into(), 220)],
            current: Some("tier A champions".into()),
            key_error: None,
            skipped_pairs: vec!["Olaf UTILITY".into(), "Teemo JUNGLE".into()],
        }
    }
    fn storage_status(&self) -> StorageStatus {
        let parts = vec![
            ("counts".to_string(), 210_000_000u64),
            ("records".into(), 380_000_000),
            ("validation".into(), 24_000_000),
            ("checkpoints".into(), 18_000_000),
            ("cache".into(), 12_000_000),
            ("ddragon".into(), 9_000_000),
            ("log".into(), 2_000_000),
        ];
        StorageStatus {
            budget_bytes: self.settings.lock().disk_budget_bytes,
            used_bytes: parts.iter().map(|p| p.1).sum(),
            parts,
            patches: vec![Patch { major: 16, minor: 19 }, Patch { major: 16, minor: 18 }],
            last_cleanup_freed_bytes: Some(41_000_000),
        }
    }
    fn volumes(&self) -> Vec<PairVolume> {
        let now = bf_core::util::now_ms();
        let p = Patch { major: 16, minor: 19 };
        [(103, Role::Middle, 1840, 0.6), (39, Role::Top, 920, 2.5), (64, Role::Jungle, 2310, 1.0), (222, Role::Bottom, 2960, 0.3), (427, Role::Jungle, 41, 30.0)]
            .iter()
            .map(|&(champion, role, games, hours)| PairVolume {
                champion,
                role,
                patch: p,
                games,
                wins: games / 2,
                newest_ms: now - (hours * 3_600_000.0) as i64,
            })
            .collect()
    }
    fn set_activity(&self, activity: Activity) {
        self.activity.lock().push(activity);
    }
    fn settings(&self) -> AppSettings {
        self.settings.lock().clone()
    }
    fn update_settings(&self, patch: &SettingsPatch) -> anyhow::Result<AppSettings> {
        let mut s = self.settings.lock();
        patch.apply(&mut s);
        Ok(s.clone())
    }
    fn set_account(&self, account: &OwnAccount) {
        *self.account.lock() = Some(account.clone());
    }
    fn game_ended(&self) {
        *self.games_ended.lock() += 1;
    }
    /// Writes a small stand-in file (not a real archive): the routes' contract, not the format, is tested here.
    fn export_backup(&self, path: &std::path::Path, include_key: bool) -> anyhow::Result<bf_store::backup::ExportReport> {
        if path.extension().and_then(|e| e.to_str()) != Some("zip") {
            anyhow::bail!("The backup file must end in .zip");
        }
        std::fs::write(path, b"sample backup")?;
        Ok(bf_store::backup::ExportReport { path: path.to_path_buf(), bytes: 13, includes_key: include_key, files: 1, millis: 0 })
    }
    fn stage_import(&self, path: &std::path::Path) -> anyhow::Result<bf_store::backup::ImportSummary> {
        if !path.is_file() {
            anyhow::bail!("No file at {}", path.display());
        }
        let s = bf_store::backup::ImportSummary { version: "1.0.0".into(), created_ms: 0, includes_key: false, database_bytes: 13, checkpoint_files: 0 };
        *self.pending.lock() = Some(s.clone());
        Ok(s)
    }
    fn pending_import(&self) -> Option<bf_store::backup::ImportSummary> {
        self.pending.lock().clone()
    }
    fn cancel_import(&self) -> anyhow::Result<()> {
        *self.pending.lock() = None;
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------ learner

pub struct FakeLearner {
    pub checkpoints: Mutex<Vec<CheckpointInfo>>,
    pub log: Mutex<Vec<String>>,
    pub activity: Mutex<Vec<Activity>>,
    pub learning: Mutex<bool>,
}

impl Default for FakeLearner {
    fn default() -> Self {
        let cp = |v: &str, kind: &str, loss: f64, active: bool| CheckpointInfo {
            version: v.into(),
            kind: kind.into(),
            created_ms: 1_764_000_000_000,
            metrics: Metrics { games: 2000, matches: 300, log_loss: loss, brier: loss * 0.36, calibration: vec![0.021, 0.018, 0.024, 0.031] },
            records_seen: 12_000,
            active,
        };
        FakeLearner {
            checkpoints: Mutex::new(vec![cp("v3", "logistic", 0.6702, true), cp("v2", "logistic", 0.6719, false), cp("v1", "logistic", 0.6788, false)]),
            log: Mutex::new(vec!["v4 rejected: log loss 0.6712 > 0.6702".into(), "v3 accepted: log loss 0.6719 → 0.6702".into()]),
            activity: Mutex::new(vec![]),
            learning: Mutex::new(true),
        }
    }
}

impl LearnerHandle for FakeLearner {
    fn status(&self) -> ModelStatus {
        ModelStatus {
            learning: *self.learning.lock(),
            backend: "cpu".into(),
            checkpoints: self.checkpoints.lock().clone(),
            log: self.log.lock().clone(),
            team: Some(bf_core::build::TeamProgress { awake: false, games: 12_345, threshold: 200_000, in_force: None, backlog: 0 }),
        }
    }
    fn rollback(&self, version: &str) -> anyhow::Result<ModelStatus> {
        {
            let mut cps = self.checkpoints.lock();
            if !cps.iter().any(|c| c.version == version) {
                anyhow::bail!("No checkpoint {version}");
            }
            for c in cps.iter_mut() {
                c.active = c.version == version;
            }
        }
        self.log.lock().insert(0, format!("rolled back to {version}"));
        Ok(self.status())
    }
    fn set_activity(&self, activity: Activity) {
        self.activity.lock().push(activity);
    }
    fn set_learning(&self, on: bool) {
        *self.learning.lock() = on;
    }
}

// ------------------------------------------------------------------------------------------------ key

/// The key in memory. `irelia_key` is what "Irelia's file" holds for the import (tests: a fake key).
pub struct FakeKeys {
    pub offer: Mutex<KeyOffer>,
    pub key: Mutex<Option<String>>,
    pub irelia_key: Mutex<Option<String>>,
    pub error: Mutex<Option<String>>,
    pub extra: Mutex<Vec<String>>,
}

impl FakeKeys {
    pub fn new(offer: KeyOffer) -> FakeKeys {
        FakeKeys { offer: Mutex::new(offer), key: Mutex::new(None), irelia_key: Mutex::new(None), error: Mutex::new(None), extra: Mutex::new(vec![]) }
    }
}

impl KeyHandle for FakeKeys {
    fn irelia_offer(&self) -> KeyOffer {
        self.offer.lock().clone()
    }
    fn import_irelia_key(&self) -> anyhow::Result<()> {
        match self.irelia_offer() {
            KeyOffer::Available => {}
            KeyOffer::Missing => anyhow::bail!("Irelia Companion's settings were not found."),
            KeyOffer::Empty => anyhow::bail!("Irelia Companion has no API key set."),
            KeyOffer::Unreadable { reason } => anyhow::bail!("Irelia Companion's settings could not be read: {reason}"),
        }
        let k = self.irelia_key.lock().clone().ok_or_else(|| anyhow::anyhow!("Irelia Companion has no API key set."))?;
        *self.key.lock() = Some(k);
        *self.error.lock() = None;
        Ok(())
    }
    fn set_key(&self, key: &str) -> anyhow::Result<()> {
        *self.key.lock() = Some(key.to_string());
        *self.error.lock() = None;
        Ok(())
    }
    fn key_masked(&self) -> String {
        self.key.lock().as_deref().map(mask_key).unwrap_or_default()
    }
    fn key_error(&self) -> Option<String> {
        self.error.lock().clone()
    }
    fn extra_keys_masked(&self) -> Vec<String> {
        self.extra.lock().iter().map(|k| mask_key(k)).collect()
    }
    fn add_extra_key(&self, key: &str) -> anyhow::Result<()> {
        let mut extra = self.extra.lock();
        if extra.iter().any(|k| k == key) || self.key.lock().as_deref() == Some(key) {
            anyhow::bail!("That key is already in use");
        }
        extra.push(key.to_string());
        Ok(())
    }
    fn remove_extra_key(&self, index: usize) -> anyhow::Result<()> {
        let mut extra = self.extra.lock();
        if index >= extra.len() {
            anyhow::bail!("No extra key {}", index + 1);
        }
        extra.remove(index);
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------ builds

/// Sample builds for any champion: fixed, valid rune pages and items rotated by champion id, champion class from
/// Data Dragon when loaded. Shapes and flags match what the learner returns; the numbers mean nothing.
pub struct StubBuildSource {
    pub data: StaticSlot,
}

const PAGES: [(&str, &str); 5] = [
    ("8000:8010,9111,9104,8299/8400:8444,8473/5008,5008,5011", "Conqueror + Resolve"),
    ("8400:8437,8446,8444,8242/8000:9111,9104/5008,5011,5013", "Grasp of the Undying + Precision"),
    ("8100:8112,8139,8140,8135/8300:8304,8347/5008,5008,5001", "Electrocute + Inspiration"),
    ("8200:8214,8226,8210,8237/8300:8304,8345/5008,5008,5001", "Summon Aery + Inspiration"),
    ("8000:8021,9111,9104,8014/8200:8226,8237/5005,5008,5011", "Fleet Footwork + Sorcery"),
];

struct Kit {
    pages: [usize; 3],
    start: &'static [&'static [i64]],
    first: &'static [i64],
    late: &'static [i64],
    boots: &'static [i64],
}

fn kit(class: &str) -> Kit {
    match class {
        "Mage" => Kit { pages: [3, 2, 0], start: &[&[1056, 2003, 2003], &[1082, 2003, 2003]], first: &[6655, 3157, 4645], late: &[3089, 3135, 3157, 3165], boots: &[3020, 3158] },
        "Marksman" => Kit { pages: [4, 0, 2], start: &[&[1055, 2003], &[1083]], first: &[3031, 3094, 3036], late: &[3036, 3026, 3033, 3094], boots: &[3006, 3009] },
        "Tank" | "Support" => Kit { pages: [1, 3, 0], start: &[&[1054, 2003], &[1055, 2003]], first: &[3742, 3143, 3075], late: &[3065, 4401, 3110, 3075], boots: &[3047, 3111] },
        _ => Kit { pages: [0, 1, 2], start: &[&[1055, 2003], &[1054, 2003]], first: &[3078, 3074, 3071], late: &[3053, 6333, 3026, 3071, 6609], boots: &[3047, 3111] },
    }
}

fn role_of_class(class: &str) -> Role {
    match class {
        "Marksman" => Role::Bottom,
        "Support" => Role::Utility,
        "Mage" | "Assassin" => Role::Middle,
        _ => Role::Top,
    }
}

impl StubBuildSource {
    fn class(&self, champion: i64) -> String {
        self.data.read().as_ref().and_then(|d| d.champion(champion).and_then(|c| c.tags.first().cloned())).unwrap_or_else(|| "Fighter".into())
    }
    fn item_name(&self, id: i64) -> String {
        self.data.read().as_ref().and_then(|d| d.item(id).map(|i| i.name.clone())).unwrap_or_else(|| format!("Item {id}"))
    }

    fn option(&self, option: BuildOption, label: String, games: u32, rank: usize) -> RankedOption {
        let win_rate = 0.535 - rank as f64 * 0.012;
        RankedOption {
            option,
            label,
            games,
            wins: (games as f64 * win_rate).round() as u32,
            weighted_games: games as f64 * 0.8,
            win_rate,
            wpa: 0.021 - rank as f64 * 0.011,
            predicted_win: None,
            basis: if games < 60 { Basis::Champion } else { Basis::Lane },
            thin: games < 60,
        }
    }
}

impl BuildSource for StubBuildSource {
    fn recommend(&self, ctx: &DraftContext) -> anyhow::Result<BuildRecommendation> {
        let class = self.class(ctx.champion);
        let role = ctx.role.unwrap_or_else(|| role_of_class(&class));
        let k = kit(&class);
        let total = 120 + (ctx.champion.unsigned_abs() as u32 * 37) % 1900;
        let share = |rank: usize| (total as f64 * [0.46, 0.27, 0.12, 0.08, 0.05][rank.min(4)]) as u32;
        let names = |ids: &[i64]| ids.iter().map(|i| self.item_name(*i)).collect::<Vec<_>>();
        let mut sections = vec![];
        let page = |i: usize| PAGES[k.pages[i]];
        sections.push(Section {
            kind: OptionKind::RunePage,
            options: (0..3).map(|i| self.option(BuildOption { kind: OptionKind::RunePage, key: page(i).0.into() }, page(i).1.into(), share(i), i)).collect(),
        });
        sections.push(Section {
            kind: OptionKind::Summoners,
            options: [(4, 14, "Flash + Ignite"), (4, 12, "Flash + Teleport")]
                .iter()
                .enumerate()
                .map(|(i, (a, b, l))| self.option(BuildOption::summoners(*a, *b), (*l).into(), share(i), i))
                .collect(),
        });
        sections.push(Section {
            kind: OptionKind::SkillMax,
            options: [[1u8, 3, 2, 1, 1, 4, 1, 3, 1, 3, 4], [3, 1, 2, 3, 3, 4, 3, 1, 3, 1, 4]]
                .iter()
                .enumerate()
                .filter_map(|(i, o)| BuildOption::skill_max(o).map(|b| (i, b)))
                .map(|(i, b)| {
                    let l = b.key.replace('>', " > ");
                    self.option(b, l, share(i), i)
                })
                .collect(),
        });
        sections.push(Section {
            kind: OptionKind::StartingItems,
            options: k
                .start
                .iter()
                .enumerate()
                .filter_map(|(i, s)| BuildOption::starting_items(s).map(|b| (i, b, names(s).join(" + "))))
                .map(|(i, b, l)| self.option(b, l, share(i), i))
                .collect(),
        });
        sections.push(Section {
            kind: OptionKind::CoreItems,
            options: (0..2)
                .filter_map(|i| {
                    let mut v = k.first.to_vec();
                    v.rotate_left(i);
                    BuildOption::core_items(&v).map(|b| (i, b, names(&v[..3]).join(" > ")))
                })
                .map(|(i, b, l)| self.option(b, l, share(i), i))
                .collect(),
        });
        sections.push(Section {
            kind: OptionKind::Boots,
            options: k.boots.iter().enumerate().map(|(i, b)| self.option(BuildOption::boots(*b), self.item_name(*b), share(i), i)).collect(),
        });
        sections.push(Section {
            kind: OptionKind::LateItem,
            options: k.late.iter().enumerate().map(|(i, b)| self.option(BuildOption::late_item(*b), self.item_name(*b), share(i + 1), i)).collect(),
        });
        let opponent = ctx.enemies.iter().find(|e| e.1 == Some(role)).map(|e| e.0);
        Ok(BuildRecommendation {
            champion: ctx.champion,
            role,
            patch: ctx.patch,
            tier: Tier::B,
            lane_opponent: opponent,
            enemy_profile: None,
            games: total,
            sections,
            model: None,
            notes: vec![STUB_NOTE.into()],
        })
    }

    fn live(&self, ctx: &LiveContext) -> anyhow::Result<LiveAdvice> {
        let class = self.class(ctx.draft.champion);
        let k = kit(&class);
        let owned = |id: i64| ctx.my_items.contains(&id);
        let next_buys: Vec<RankedOption> = k
            .first
            .iter()
            .chain(k.late.iter())
            .filter(|i| !owned(**i))
            .take(3)
            .enumerate()
            .map(|(i, id)| self.option(BuildOption::late_item(*id), self.item_name(*id), 400 - i as u32 * 120, i))
            .collect();
        // Enemy classes from Data Dragon: mostly magic or physical damage, and healing, each suggest an answer.
        let classes: Vec<String> = ctx.draft.enemies.iter().map(|e| self.class(e.0)).collect();
        let magic = classes.iter().filter(|c| *c == "Mage").count();
        let mut counters = vec![];
        let mr = if class == "Mage" { 3102 } else { 3156 };
        let armor = if class == "Mage" { 3157 } else { 6333 };
        if magic >= 2 && !owned(mr) {
            counters.push((self.option(BuildOption::late_item(mr), self.item_name(mr), 210, 0), "Enemy damage is mostly magic".to_string()));
        }
        if magic < 2 && !owned(armor) {
            counters.push((self.option(BuildOption::late_item(armor), self.item_name(armor), 190, 0), "Enemy damage is mostly physical".to_string()));
        }
        let anti_heal = if class == "Mage" { 3165 } else { 3123 };
        if !owned(anti_heal) {
            counters.push((self.option(BuildOption::late_item(anti_heal), self.item_name(anti_heal), 80, 1), "Anti-heal against sustain".to_string()));
        }
        // Prices at full cost (the stand-in doesn't walk recipes), against the user's gold.
        let gold = ctx.my_gold.max(0.0) as u32;
        let buys = next_buys
            .iter()
            .chain(counters.iter().map(|c| &c.0))
            .map(|o| {
                let id = o.option.item_ids()[0];
                let cost = self.data.read().as_ref().and_then(|d| d.item(id).map(|i| i.gold_total as u32)).unwrap_or(3000);
                BuyNote { item: id, cost, short: cost.saturating_sub(gold), part: None, part_cost: None, reason: None }
            })
            .collect();
        // 1.5.0: the plan in buying order: the kit's first item, its first boots, then its late items, minus what is owned.
        let price = |id: i64| self.data.read().as_ref().and_then(|d| d.item(id).map(|i| i.gold_total as u32)).unwrap_or(3000);
        let worth = |id: &i64| self.data.read().as_ref().and_then(|d| d.item(*id).map(|i| i.gold_total)).unwrap_or(0.0);
        let start = (ctx.game_time_s < 120.0 && ctx.my_items.iter().all(|i| worth(i) <= 0.0))
            .then(|| BuildOption::starting_items(k.start[0]).map(|o| self.option(o, "Starting items".into(), 400, 0)))
            .flatten();
        let has_boots = k.boots.iter().any(|b| owned(*b));
        let done: Vec<i64> = ctx.my_items.iter().copied().filter(|i| k.first.contains(i) || k.late.contains(i) || k.boots.contains(i)).collect();
        let mut order: Vec<(i64, &str)> = vec![];
        for id in k.first.iter().take(1).chain(if has_boots { [].iter() } else { k.boots[..1].iter() }).chain(k.first.iter().skip(1)).chain(k.late.iter()) {
            if !owned(*id) && !order.iter().any(|o| o.0 == *id) {
                order.push((*id, if k.boots.contains(id) { "Boots" } else { "Most built next" }));
            }
        }
        order.truncate(6usize.saturating_sub(done.len()));
        let mut left = gold;
        let steps = order
            .into_iter()
            .map(|(id, why)| {
                let cost = price(id);
                let buy = BuyNote { item: id, cost, short: cost.saturating_sub(left), part: None, part_cost: None, reason: None };
                left = left.saturating_sub(cost);
                PlanStep { item: id, label: self.item_name(id), buy, why: why.into(), counter: false, alternatives: vec![] }
            })
            .collect();
        let plan = Some(BuildPlan { start, done, steps });
        Ok(LiveAdvice { next_buys, counters, predicted_win: None, notes: vec![STUB_NOTE.into()], buys, plan })
    }
}
