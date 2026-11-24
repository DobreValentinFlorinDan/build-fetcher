//! Synthetic match-v5 matches and timelines at scale: any champion in any role, tens of thousands of varied games,
//! the same games again from the same seed. Game `i` depends only on (seed, i), so games can be made in any order or
//! in parallel.
//!
//! What varies, and why:
//! - champions per role come from Data Dragon tags with a skewed popularity, so some champion-role pairs are popular
//!   and many are thin (the fetcher's priorities and the learner's fall-backs need both);
//! - each champion-role has three builds (rune page, summoners, skill max, start, item order, boots) with hidden
//!   qualities (+0.25 / 0 / -0.25 on the win logit), so counts and models have a real, known signal to find;
//! - a share of games (`timeline_share`) come with a full timeline: frames, positions in the right lane area for
//!   minutes 1-14, purchases (with an occasional undo and sell), skill-ups, kills, dragons and towers;
//! - `noise_share` of games carry one defect a data-quality filter must catch (`SynthGame::defect` says which).

use crate::ddragon::StaticData;
use crate::engine::items::{is_completed_item, is_tier2_boots};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// splitmix64: tiny, fast and good enough for test data (no dependency).
#[derive(Clone, Debug)]
pub struct SynthRng(u64);

impl SynthRng {
    pub fn new(seed: u64) -> SynthRng {
        SynthRng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Uniform in 0..n (0 when n is 0).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
    /// Uniform in lo..hi.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo).max(1) as usize) as i64
    }
}

fn mix(a: u64, b: u64) -> u64 {
    SynthRng::new(a.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ b).next_u64()
}

pub const SYNTH_POSITIONS: [&str; 5] = ["TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY"];

#[derive(Clone, Debug)]
pub struct SynthOptions {
    pub seed: u64,
    /// Share of games with a timeline.
    pub timeline_share: f64,
    /// match-v5 gameVersions, oldest first; game i plays on versions[i / games_per_version] (the last one after).
    pub versions: Vec<String>,
    pub games_per_version: u64,
    pub platforms: Vec<String>,
    /// gameCreation of game 0, and the gap between games.
    pub start_ms: i64,
    pub spacing_ms: i64,
    /// A (champion, teamPosition) put into `featured_share` of the games (team 100).
    pub featured: Option<(i64, String)>,
    pub featured_share: f64,
    /// Share of games with one defect a data-quality filter must catch.
    pub noise_share: f64,
    /// Distinct puuids per platform (players recur across games, as in a real ladder).
    pub players_per_platform: u64,
}

impl Default for SynthOptions {
    fn default() -> Self {
        SynthOptions {
            seed: 1,
            timeline_share: 0.1,
            versions: vec!["16.18.704.1234".into(), "16.19.712.4471".into()],
            games_per_version: 25_000,
            platforms: vec!["EUN1".into(), "EUW1".into(), "KR".into()],
            start_ms: super::date_utc(2026, 8, 1),
            spacing_ms: 60_000,
            featured: None,
            featured_share: 0.0,
            noise_share: 0.0,
            players_per_platform: 20_000,
        }
    }
}

pub struct SynthGame {
    pub id: String,
    pub platform: String,
    pub m: Value,
    pub timeline: Option<Value>,
    /// The defect put in ("queue", "remake", "leaver", "roles", "malformed"), if any.
    pub defect: Option<&'static str>,
}

/// One of a champion-role's builds.
#[derive(Clone, Debug)]
pub struct SynthBuild {
    pub style: i64,
    pub perks: [i64; 4],
    pub sub_style: i64,
    pub sub_perks: [i64; 2],
    pub shards: [i64; 3],
    pub summoners: [i64; 2],
    /// Skill slots (1-3) in max order.
    pub skill_max: [u8; 3],
    pub start: Vec<i64>,
    /// Completed items in purchase order.
    pub items: Vec<i64>,
    pub boots: i64,
    /// Added to the team's win logit.
    pub quality: f64,
}

pub struct Synth {
    pub o: SynthOptions,
    data: Arc<StaticData>,
    /// Per role: (champion, cumulative weight).
    pools: Vec<Vec<(i64, f64)>>,
    items_phys: Vec<i64>,
    items_magic: Vec<i64>,
    items_tank: Vec<i64>,
    boots: Vec<i64>,
    /// (style, slots of rune ids).
    trees: Vec<(i64, Vec<Vec<i64>>)>,
}

const BUILD_WEIGHTS: [f64; 3] = [0.5, 0.3, 0.2];
const BUILD_QUALITY: [f64; 3] = [0.25, 0.0, -0.25];

/// Lane spots on Summoner's Rift (team 100's jungle; team 200's is mirrored).
fn lane_spot(role: usize, team: u16) -> (f64, f64) {
    match role {
        0 => (1500.0, 12500.0),
        1 if team == 100 => (3800.0, 8200.0),
        1 => (11000.0, 6600.0),
        2 => (7400.0, 7400.0),
        3 => (12500.0, 1500.0),
        _ => (11800.0, 1600.0),
    }
}

impl Synth {
    pub fn new(o: SynthOptions, data: Arc<StaticData>) -> Synth {
        let mut rng = SynthRng::new(o.seed ^ 0xC0FFEE);
        let mut pools: Vec<Vec<i64>> = vec![vec![]; 5];
        for c in data.champions.values() {
            let has = |t: &str| c.tags.iter().any(|x| x == t);
            if has("Fighter") || has("Tank") {
                pools[0].push(c.id);
            }
            if has("Fighter") || has("Assassin") || has("Tank") {
                pools[1].push(c.id);
            }
            if has("Mage") || has("Assassin") {
                pools[2].push(c.id);
            }
            if has("Marksman") {
                pools[3].push(c.id);
            }
            if has("Support") || (has("Mage") && c.info.difficulty <= 5.0) || (has("Tank") && c.info.defense >= 8.0) {
                pools[4].push(c.id);
            }
        }
        // Skewed popularity: a shuffled order with Zipf-like weights.
        let pools = pools
            .into_iter()
            .map(|mut p| {
                for i in (1..p.len()).rev() {
                    let j = rng.below(i + 1);
                    p.swap(i, j);
                }
                let mut acc = 0.0;
                p.into_iter()
                    .enumerate()
                    .map(|(k, c)| {
                        acc += 1.0 / ((k + 1) as f64).powf(0.9);
                        (c, acc)
                    })
                    .collect()
            })
            .collect();
        let mut items_phys = vec![];
        let mut items_magic = vec![];
        let mut items_tank = vec![];
        let mut boots = vec![];
        for it in data.items.values() {
            if !it.on_summoners_rift || !it.in_store || it.id >= 10_000 {
                continue;
            }
            let has = |t: &str| it.tags.iter().any(|x| x == t);
            if is_tier2_boots(Some(it)) {
                boots.push(it.id);
            } else if is_completed_item(Some(it)) {
                if has("SpellDamage") {
                    items_magic.push(it.id);
                } else if has("Damage") || has("CriticalStrike") || has("AttackSpeed") {
                    items_phys.push(it.id);
                } else if has("Armor") || has("SpellBlock") || has("Health") {
                    items_tank.push(it.id);
                }
            }
        }
        let trees = data.runes.iter().map(|t| (t.id, t.slots.iter().map(|s| s.runes.iter().map(|r| r.id).collect()).collect())).collect();
        Synth { o, data, pools, items_phys, items_magic, items_tank, boots, trees }
    }

    /// Build `b` (0-2) of a champion in a role: fixed for the seed.
    pub fn build(&self, champion: i64, role: usize, b: usize) -> SynthBuild {
        let mut r = SynthRng::new(mix(self.o.seed, (champion as u64) << 8 | (role as u64) << 4 | b as u64));
        let (ti, si) = {
            let t = r.below(self.trees.len());
            let mut s = r.below(self.trees.len());
            if s == t {
                s = (s + 1) % self.trees.len();
            }
            (t, s)
        };
        let (style, slots) = &self.trees[ti];
        let pick = |r: &mut SynthRng, v: &Vec<i64>| v[r.below(v.len())];
        let perks = [pick(&mut r, &slots[0]), pick(&mut r, &slots[1]), pick(&mut r, &slots[2]), pick(&mut r, &slots[3])];
        let (sub_style, sub_slots) = &self.trees[si];
        let a = 1 + r.below(3);
        let bslot = 1 + (a + r.below(2)) % 3;
        let (a, bslot) = if a < bslot { (a, bslot) } else { (bslot, a) };
        let sub_perks = [pick(&mut r, &sub_slots[a]), pick(&mut r, &sub_slots[bslot])];
        let shards = [[5005, 5008, 5007][r.below(3)], [5008, 5010, 5001][r.below(3)], [5011, 5001, 5013][r.below(3)]];
        let second = match role {
            1 => 11,
            3 => [7, 21, 6][r.below(3)],
            4 => [14, 3, 7][r.below(3)],
            0 => [12, 14, 6][r.below(3)],
            _ => [14, 12, 21][r.below(3)],
        };
        let skill_max = [[1, 3, 2], [1, 2, 3], [3, 1, 2], [2, 1, 3]][r.below(4)];
        let start = match role {
            0 => vec![[1055, 1054][r.below(2)], 2003],
            1 => vec![[1101, 1102, 1103][r.below(3)], 2003],
            2 => vec![1056, 2003, 2003],
            3 => vec![[1055, 1083][r.below(2)], 2003],
            _ => vec![3865, 2003],
        };
        let c = self.data.champion(champion);
        let tank = c.map(|c| c.tags.iter().any(|t| t == "Tank")).unwrap_or(false);
        let magic = c.map(|c| c.info.magic > c.info.attack).unwrap_or(false);
        let pool = if magic && !self.items_magic.is_empty() {
            &self.items_magic
        } else if (tank || role == 4) && !self.items_tank.is_empty() {
            &self.items_tank
        } else if !self.items_phys.is_empty() {
            &self.items_phys
        } else {
            &self.items_tank
        };
        let mut items: Vec<i64> = Vec::new();
        while items.len() < 6.min(pool.len()) {
            let it = pool[r.below(pool.len())];
            if !items.contains(&it) {
                items.push(it);
            }
        }
        let boots = if self.boots.is_empty() { 0 } else { self.boots[r.below(self.boots.len())] };
        let qi = (b + (mix(self.o.seed, champion as u64 * 7 + role as u64) % 3) as usize) % 3;
        SynthBuild { style: *style, perks, sub_style: *sub_style, sub_perks, shards, summoners: [4, second], skill_max, start, items, boots, quality: BUILD_QUALITY[qi] }
    }

    fn champion(&self, r: &mut SynthRng, role: usize) -> i64 {
        let pool = &self.pools[role];
        if pool.is_empty() {
            return *self.data.champions.keys().nth(r.below(self.data.champions.len())).unwrap_or(&1);
        }
        let x = r.unit() * pool.last().unwrap().1;
        let i = pool.partition_point(|(_, acc)| *acc <= x).min(pool.len() - 1);
        pool[i].0
    }

    pub fn version_of(&self, i: u64) -> &str {
        let k = ((i / self.o.games_per_version.max(1)) as usize).min(self.o.versions.len() - 1);
        &self.o.versions[k]
    }

    /// Game `i`.
    pub fn game(&self, i: u64) -> SynthGame {
        let mut r = SynthRng::new(mix(self.o.seed, i));
        let platform = self.o.platforms[r.below(self.o.platforms.len())].clone();
        let id = format!("{platform}_{}", 7_000_000_000u64 + i);
        // Ten distinct champions, team 100 then 200, each in TOP..UTILITY order.
        let mut champs = [0i64; 10];
        let featured = self.o.featured.clone().filter(|_| r.chance(self.o.featured_share));
        for k in 0..10 {
            let role = k % 5;
            if let Some((c, pos)) = &featured {
                if k < 5 && SYNTH_POSITIONS[role] == pos {
                    champs[k] = *c;
                    continue;
                }
            }
            loop {
                let c = self.champion(&mut r, role);
                let taken = champs.contains(&c) || featured.as_ref().map(|f| f.0 == c).unwrap_or(false);
                if !taken {
                    champs[k] = c;
                    break;
                }
            }
        }
        let builds: Vec<SynthBuild> = (0..10)
            .map(|k| {
                let x = r.unit();
                let b = if x < BUILD_WEIGHTS[0] { 0 } else if x < BUILD_WEIGHTS[0] + BUILD_WEIGHTS[1] { 1 } else { 2 };
                self.build(champs[k], k % 5, b)
            })
            .collect();
        let logit: f64 = builds[..5].iter().map(|b| b.quality).sum::<f64>() - builds[5..].iter().map(|b| b.quality).sum::<f64>();
        let winner: u16 = if r.unit() < 1.0 / (1.0 + (-logit).exp()) { 100 } else { 200 };
        let minutes = r.range(20, 38);
        let mut puuids: Vec<String> = Vec::new();
        while puuids.len() < 10 {
            let p = format!("{}-player-{:06}", platform.to_lowercase(), r.below(self.o.players_per_platform as usize));
            if !puuids.contains(&p) {
                puuids.push(p);
            }
        }
        let defect: Option<&'static str> = r.chance(self.o.noise_share).then(|| ["queue", "remake", "leaver", "roles", "malformed"][r.below(5)]);
        let with_timeline = r.chance(self.o.timeline_share);
        let creation = self.o.start_ms + i as i64 * self.o.spacing_ms;
        let duration_s = if defect == Some("remake") { 200 } else { minutes * 60 + r.range(0, 60) };
        let afk = if defect == Some("leaver") { Some(r.below(10)) } else { None };
        // Per player: which completed items they finished before the end, and their skill-ups.
        let mut participants = Vec::new();
        let mut plans = Vec::new();
        for k in 0..10 {
            let team: u16 = if k < 5 { 100 } else { 200 };
            let role = k % 5;
            let b = &builds[k];
            let win = team == winner;
            let mins = duration_s as f64 / 60.0;
            let done = (((mins - 8.0) / 7.5).floor().max(0.0) as usize).min(b.items.len()).min(5);
            let level = ((mins * 0.62) as i64 + 1).clamp(6, 18) as usize;
            let skills = skill_order(&b.skill_max, level);
            let is_afk = afk == Some(k);
            let gpm = [380.0, 370.0, 390.0, 400.0, 260.0][role] * if win { 1.08 } else { 0.94 };
            let gold = if is_afk { 500.0 + 122.0 * mins } else { 500.0 + gpm * mins + r.range(-400, 400) as f64 };
            let cs_pm = [7.0, 6.0, 7.5, 8.0, 1.0][role];
            let cs = if is_afk { 0 } else { (cs_pm * mins) as i64 + r.range(-20, 20) };
            let cs = cs.max(0);
            let mut final_items: Vec<i64> = b.items[..done].to_vec();
            if mins > 10.0 {
                final_items.push(b.boots);
            }
            while final_items.len() < 6 {
                final_items.push(0);
            }
            final_items.push(3340);
            let c = self.data.champion(champs[k]);
            let (att, mag) = c.map(|c| (c.info.attack, c.info.magic)).unwrap_or((5.0, 5.0));
            let dmg = if is_afk { 0.0 } else { (800.0 + r.range(0, 600) as f64) * mins };
            let phys = dmg * (att + 0.5) / (att + mag + 1.0);
            let position = SYNTH_POSITIONS[role];
            let mut p = json!({
                "puuid": puuids[k], "participantId": k + 1, "championId": champs[k], "teamId": team,
                "teamPosition": position, "individualPosition": position, "win": win,
                "summoner1Id": b.summoners[0], "summoner2Id": b.summoners[1],
                "kills": if is_afk { 0 } else { r.range(0, 12) }, "deaths": r.range(0, 10), "assists": if is_afk { 0 } else { r.range(0, 15) },
                "item0": final_items[0], "item1": final_items[1], "item2": final_items[2], "item3": final_items[3],
                "item4": final_items[4], "item5": final_items[5], "item6": final_items[6],
                "perks": {
                    "statPerks": { "offense": b.shards[0], "flex": b.shards[1], "defense": b.shards[2] },
                    "styles": [
                        { "description": "primaryStyle", "style": b.style, "selections": b.perks.iter().map(|x| json!({ "perk": x })).collect::<Vec<_>>() },
                        { "description": "subStyle", "style": b.sub_style, "selections": b.sub_perks.iter().map(|x| json!({ "perk": x })).collect::<Vec<_>>() },
                    ],
                },
                "champLevel": level, "goldEarned": gold as i64,
                "totalMinionsKilled": if role == 1 { cs / 5 } else { cs }, "neutralMinionsKilled": if role == 1 { cs * 4 / 5 } else { 0 },
                "physicalDamageDealtToChampions": phys as i64, "magicDamageDealtToChampions": (dmg - phys) as i64,
                "trueDamageDealtToChampions": (dmg * 0.05) as i64,
                "timePlayed": if is_afk { 300 } else { duration_s },
                "gameEndedInEarlySurrender": defect == Some("remake"), "gameEndedInSurrender": false, "teamEarlySurrendered": false,
            });
            if defect == Some("roles") && k == 1 {
                p["teamPosition"] = json!("TOP");
            }
            participants.push(p);
            plans.push((done, skills, is_afk));
        }
        if defect == Some("malformed") {
            participants.pop();
        }
        let m = json!({
            "metadata": { "matchId": id, "participants": puuids },
            "info": {
                "gameVersion": if defect == Some("malformed") { Value::Null } else { json!(self.version_of(i)) },
                "queueId": if defect == Some("queue") { 440 } else { 420 },
                "mapId": 11,
                "gameCreation": creation, "gameStartTimestamp": creation + 40_000,
                "gameEndTimestamp": creation + 40_000 + duration_s * 1000,
                "gameDuration": duration_s, "endOfGameResult": "GameComplete",
                "platformId": platform,
                "participants": participants,
            },
        });
        let timeline = with_timeline.then(|| self.timeline(&mut r, &puuids, &builds, &plans, duration_s, winner));
        SynthGame { id, platform, m, timeline, defect }
    }

    fn timeline(&self, r: &mut SynthRng, puuids: &[String], builds: &[SynthBuild], plans: &[(usize, Vec<u8>, bool)], duration_s: i64, winner: u16) -> Value {
        let minutes = duration_s / 60;
        let mut frames = Vec::new();
        let mut gold = [500.0f64; 10];
        let mut xp = [0.0f64; 10];
        let mut cs = [0i64; 10];
        // Purchase events per player as (ms, event).
        let mut item_events: Vec<(i64, Value)> = Vec::new();
        for k in 0..10 {
            let pid = k as i64 + 1;
            let b = &builds[k];
            let (done, _, afk) = &plans[k];
            for (j, it) in b.start.iter().enumerate() {
                item_events.push((5_000 + j as i64 * 900 + r.range(0, 400), json!({ "type": "ITEM_PURCHASED", "participantId": pid, "itemId": it })));
            }
            if *afk {
                continue;
            }
            // A misclick undone right away now and then: undos must be applied.
            if r.chance(0.05) {
                let t = 3 * 60_000 + r.range(0, 30_000);
                item_events.push((t, json!({ "type": "ITEM_PURCHASED", "participantId": pid, "itemId": 1036 })));
                item_events.push((t + 800, json!({ "type": "ITEM_UNDO", "participantId": pid, "beforeId": 1036, "afterId": 0, "goldGain": 350 })));
            }
            item_events.push((r.range(4, 7) * 60_000 + r.range(0, 50_000), json!({ "type": "ITEM_PURCHASED", "participantId": pid, "itemId": 1001 })));
            if minutes > 10 {
                item_events.push((r.range(9, 11) * 60_000 + r.range(0, 50_000), json!({ "type": "ITEM_PURCHASED", "participantId": pid, "itemId": b.boots })));
            }
            for (j, it) in b.items[..*done].iter().enumerate() {
                let t = ((8.0 + 7.5 * (j as f64 + 1.0)) * 60_000.0) as i64 - r.range(0, 120_000);
                item_events.push((t.min(duration_s * 1000 - 1000), json!({ "type": "ITEM_PURCHASED", "participantId": pid, "itemId": it })));
            }
            if *done >= 2 && r.chance(0.5) {
                item_events.push((((8.0 + 7.5 * 2.0) * 60_000.0) as i64 + 5_000, json!({ "type": "ITEM_SOLD", "participantId": pid, "itemId": b.start[0] })));
            }
        }
        item_events.sort_by_key(|e| e.0);
        let mut next_item = 0;
        for mm in 0..=minutes {
            let t_end = (mm + 1) * 60_000;
            let mut pf = Map::new();
            for k in 0..10 {
                let pid = k + 1;
                let team: u16 = if k < 5 { 100 } else { 200 };
                let role = k % 5;
                let afk = plans[k].2;
                let active = !afk || mm <= 2;
                if mm > 0 {
                    gold[k] += if active { [380.0, 370.0, 390.0, 400.0, 260.0][role] * if team == winner { 1.05 } else { 0.97 } } else { 122.0 };
                    if active {
                        xp[k] += [560.0, 520.0, 600.0, 480.0, 380.0][role];
                        cs[k] += [7, 6, 8, 8, 1][role];
                    }
                }
                let (x, y) = if active && (1..=14).contains(&mm) {
                    let (sx, sy) = lane_spot(role, team);
                    (sx + r.range(-450, 450) as f64, sy + r.range(-450, 450) as f64)
                } else if !active {
                    if team == 100 { (500.0, 500.0) } else { (14300.0, 14300.0) }
                } else {
                    (r.range(1000, 13800) as f64, r.range(1000, 13800) as f64)
                };
                let level = level_for_xp(xp[k]);
                pf.insert(
                    pid.to_string(),
                    json!({
                        "participantId": pid, "totalGold": gold[k] as i64, "currentGold": 200, "xp": xp[k] as i64, "level": level,
                        "minionsKilled": if role == 1 { cs[k] / 5 } else { cs[k] }, "jungleMinionsKilled": if role == 1 { cs[k] * 4 / 5 } else { 0 },
                        "position": { "x": x as i64, "y": y as i64 },
                    }),
                );
            }
            let mut events: Vec<Value> = Vec::new();
            while next_item < item_events.len() && item_events[next_item].0 < t_end {
                let (t, mut ev) = item_events[next_item].clone();
                ev["timestamp"] = json!(t);
                events.push(ev);
                next_item += 1;
            }
            // Skill-ups: one per level gained this minute.
            for k in 0..10 {
                let skills = &plans[k].1;
                let before = if mm == 0 { 0 } else { level_for_xp(xp_at(k, mm - 1, plans, winner)) as usize };
                let now = level_for_xp(xp_at(k, mm, plans, winner)) as usize;
                for lvl in before.max(0)..now.min(skills.len()) {
                    events.push(json!({ "type": "SKILL_LEVEL_UP", "timestamp": mm * 60_000 + 200 + lvl as i64, "participantId": k + 1, "skillSlot": skills[lvl], "levelUpType": "NORMAL" }));
                }
            }
            if mm >= 3 && r.chance(0.6) {
                let killer = r.range(1, 11);
                let victim = if killer <= 5 { r.range(6, 11) } else { r.range(1, 6) };
                events.push(json!({ "type": "CHAMPION_KILL", "timestamp": mm * 60_000 + 30_000, "killerId": killer, "victimId": victim, "assistingParticipantIds": [], "bounty": 300, "shutdownBounty": 0 }));
            }
            if mm > 0 && mm % 6 == 0 {
                let team = if r.chance(0.5) { 100 } else { 200 };
                events.push(json!({ "type": "ELITE_MONSTER_KILL", "timestamp": mm * 60_000 + 10_000, "killerId": if team == 100 { 2 } else { 7 }, "killerTeamId": team, "monsterType": "DRAGON", "monsterSubType": "FIRE_DRAGON" }));
            }
            if mm >= 12 && mm % 4 == 0 {
                let loser = if r.chance(0.5) { 100 } else { 200 };
                events.push(json!({ "type": "BUILDING_KILL", "timestamp": mm * 60_000 + 20_000, "killerId": if loser == 100 { 6 } else { 1 }, "teamId": loser, "buildingType": "TOWER_BUILDING", "towerType": "OUTER_TURRET", "laneType": "MID_LANE" }));
            }
            events.sort_by_key(|e| e["timestamp"].as_i64().unwrap_or(0));
            frames.push(json!({ "timestamp": mm * 60_000, "participantFrames": pf, "events": events }));
        }
        frames.push(json!({ "timestamp": duration_s * 1000, "participantFrames": frames.last().map(|f| f["participantFrames"].clone()).unwrap_or(json!({})), "events": [{ "type": "GAME_END", "timestamp": duration_s * 1000, "winningTeam": winner }] }));
        json!({
            "metadata": { "matchId": Value::Null, "participants": puuids },
            "info": {
                "frameInterval": 60_000,
                "participants": (0..10).map(|k| json!({ "participantId": k + 1, "puuid": puuids[k] })).collect::<Vec<_>>(),
                "frames": frames,
            },
        })
    }
}

/// XP a player has at the end of minute `mm` (the same arithmetic as the frames, without the randomness).
fn xp_at(k: usize, mm: i64, plans: &[(usize, Vec<u8>, bool)], _winner: u16) -> f64 {
    let role = k % 5;
    let active = if plans[k].2 { mm.min(2) } else { mm };
    active as f64 * [560.0, 520.0, 600.0, 480.0, 380.0][role]
}

/// Champion level for total XP (Riot's 280 + 100 per level curve, close enough).
fn level_for_xp(xp: f64) -> i64 {
    let mut need = 280.0;
    let mut level = 1;
    let mut left = xp;
    while level < 18 && left >= need {
        left -= need;
        need += 100.0;
        level += 1;
    }
    level
}

/// Skill slot (1-4) per level: R at 6, 11 and 16; the max order's first three levels, then max order.
pub fn skill_order(max: &[u8; 3], levels: usize) -> Vec<u8> {
    let mut points = [0u8; 4];
    let mut out = Vec::new();
    for lvl in 1..=levels.min(18) {
        let s = if lvl == 6 || lvl == 11 || lvl == 16 {
            4
        } else if lvl <= 3 {
            max[lvl - 1]
        } else {
            *max.iter().find(|s| points[(**s - 1) as usize] < 5).unwrap_or(&max[0])
        };
        points[(s - 1) as usize] += 1;
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_games_and_variety() {
        let data = crate::testkit::load_test_static_data();
        let s = Synth::new(SynthOptions { timeline_share: 0.5, noise_share: 0.2, ..Default::default() }, data.clone());
        let again = Synth::new(SynthOptions { timeline_share: 0.5, noise_share: 0.2, ..Default::default() }, data.clone());
        let a = s.game(17);
        assert_eq!(a.m, again.game(17).m);
        assert_eq!(a.timeline, again.game(17).timeline);
        let mut champs = std::collections::HashSet::new();
        let mut timelines = 0;
        for i in 0..400 {
            let g = s.game(i);
            for p in g.m["info"]["participants"].as_array().unwrap() {
                champs.insert(p["championId"].as_i64().unwrap());
            }
            timelines += g.timeline.is_some() as usize;
        }
        assert!(champs.len() > 100, "{} champions", champs.len());
        assert!((150..250).contains(&timelines), "{timelines}");
        let f = Synth::new(SynthOptions { featured: Some((39, "MIDDLE".into())), featured_share: 1.0, ..Default::default() }, data);
        let g = f.game(3);
        assert_eq!(g.m["info"]["participants"][2]["championId"], 39);
        assert_eq!(g.m["info"]["participants"][2]["teamPosition"], "MIDDLE");
        assert_eq!(skill_order(&[1, 3, 2], 9), vec![1, 3, 2, 1, 1, 4, 1, 1, 3]);
    }
}
