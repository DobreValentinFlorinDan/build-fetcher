//! Port of `server/riot/routing.ts`: static Riot routing facts.

pub const REGION_HOSTS: [(&str, &str); 4] = [
    ("AMERICAS", "americas.api.riotgames.com"),
    ("ASIA", "asia.api.riotgames.com"),
    ("EUROPE", "europe.api.riotgames.com"),
    ("SEA", "sea.api.riotgames.com"),
];

/// (platform, region, label), in the TS table's order.
pub const PLATFORMS: [(&str, &str, &str); 15] = [
    ("KR", "ASIA", "Korea"),
    ("JP1", "ASIA", "Japan"),
    ("EUW1", "EUROPE", "EU West"),
    ("EUN1", "EUROPE", "EU Nordic & East"),
    ("TR1", "EUROPE", "Turkey"),
    ("RU", "EUROPE", "Russia"),
    ("ME1", "EUROPE", "Middle East"),
    ("NA1", "AMERICAS", "North America"),
    ("BR1", "AMERICAS", "Brazil"),
    ("LA1", "AMERICAS", "LAN"),
    ("LA2", "AMERICAS", "LAS"),
    ("OC1", "SEA", "Oceania"),
    ("SG2", "SEA", "Singapore"),
    ("TW2", "SEA", "Taiwan"),
    ("VN2", "SEA", "Vietnam"),
];

/// Ranked tiers, highest first.
pub const TIERS: [&str; 10] =
    ["CHALLENGER", "GRANDMASTER", "MASTER", "DIAMOND", "EMERALD", "PLATINUM", "GOLD", "SILVER", "BRONZE", "IRON"];
pub const APEX_TIERS: [&str; 3] = ["CHALLENGER", "GRANDMASTER", "MASTER"];
pub const DIVISIONS: [&str; 4] = ["I", "II", "III", "IV"];

pub fn is_platform(p: &str) -> bool {
    PLATFORMS.iter().any(|x| x.0 == p)
}
pub fn is_tier(t: &str) -> bool {
    TIERS.contains(&t)
}
pub fn is_apex(t: &str) -> bool {
    APEX_TIERS.contains(&t)
}

pub fn region_of(p: &str) -> &'static str {
    PLATFORMS.iter().find(|x| x.0 == p).map(|x| x.1).unwrap_or("EUROPE")
}
pub fn label_of(p: &str) -> &'static str {
    PLATFORMS.iter().find(|x| x.0 == p).map(|x| x.2).unwrap_or("")
}
fn region_host(region: &str) -> &'static str {
    REGION_HOSTS.iter().find(|x| x.0 == region).map(|x| x.1).unwrap()
}

pub fn platform_host(p: &str) -> String {
    format!("{}.api.riotgames.com", p.to_lowercase())
}
pub fn regional_host(p: &str) -> &'static str {
    region_host(region_of(p))
}

/// Accepts legacy lowercase ids ("euw1") stored by older versions.
pub fn normalize_platform(p: &str) -> anyhow::Result<String> {
    let up = p.to_uppercase();
    if !is_platform(&up) {
        anyhow::bail!(
            "Unknown platform \"{}\". Known: {}",
            p,
            PLATFORMS.iter().map(|x| x.0).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(up)
}

const REGION_NAMES: [(&str, &str); 18] = [
    ("KR", "KR"),
    ("JP", "JP1"),
    ("EUW", "EUW1"),
    ("EUNE", "EUN1"),
    ("TR", "TR1"),
    ("RU", "RU"),
    ("ME", "ME1"),
    ("NA", "NA1"),
    ("BR", "BR1"),
    ("LAN", "LA1"),
    ("LAS", "LA2"),
    ("OCE", "OC1"),
    ("SG", "SG2"),
    ("PH", "SG2"),
    ("TH", "SG2"),
    ("SEA", "SG2"),
    ("TW", "TW2"),
    ("VN", "VN2"),
];

/// A platform from a platform id ("EUW1", "euw1") or the region name the Riot client shows ("EUW", "EUNE").
pub fn to_platform(value: Option<&str>) -> Option<String> {
    let v = value?.trim();
    if v.is_empty() {
        return None;
    }
    let up = v.to_uppercase();
    if is_platform(&up) {
        return Some(up);
    }
    REGION_NAMES.iter().find(|x| x.0 == up).map(|x| x.1.to_string())
}

/// Account-v1 answers on the three big regional hosts; SEA platforms use Asia's.
pub fn account_host(p: Option<&str>) -> &'static str {
    let region = p.map(region_of).unwrap_or("EUROPE");
    region_host(if region == "SEA" { "ASIA" } else { region })
}

/// Match ids are prefixed with their platform: "KR_7312..." -> KR
pub fn platform_of_match(match_id: &str) -> &str {
    match_id.split('_').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platforms_route_to_their_regional_match_v5_host() {
        assert_eq!(regional_host("KR"), "asia.api.riotgames.com");
        assert_eq!(platform_host("KR"), "kr.api.riotgames.com");
        assert_eq!(regional_host("EUW1"), "europe.api.riotgames.com");
        assert_eq!(regional_host("NA1"), "americas.api.riotgames.com");
        assert_eq!(regional_host("OC1"), "sea.api.riotgames.com");
        assert_eq!(normalize_platform("euw1").unwrap(), "EUW1");
        assert_eq!(platform_of_match("KR_8393787736"), "KR");
        assert!(normalize_platform("XX9").is_err());
    }
}
