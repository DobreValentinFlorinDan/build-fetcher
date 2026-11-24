//! Port of `server/scraper/account.ts`: your own account, for fetching your games (source "me"). The League client's
//! PUUID is not the one the Riot API takes, so the API's is looked up from your Riot ID.

use crate::riot::client::RiotApi;
use crate::riot::rate_limiter::Signal;
use crate::riot::routing::{account_host, to_platform};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClientAccount {
    pub game_name: Option<String>,
    pub tag_line: Option<String>,
    pub platform: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    Ok { puuid: String, platform: String, riot_id: String },
    Err(String),
}

/// The League client's own PUUID format (a UUID): never valid for the Riot API.
pub fn is_client_puuid(puuid: &str) -> bool {
    let parts: Vec<&str> = puuid.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    parts.len() == 5 && parts.iter().zip(lens).all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

pub async fn resolve_account<A: RiotApi>(riot: &A, a: &ClientAccount, manual_platform: Option<&str>, signal: &Signal) -> anyhow::Result<Resolved> {
    let (Some(name), Some(tag)) = (a.game_name.as_deref().filter(|s| !s.is_empty()), a.tag_line.as_deref().filter(|s| !s.is_empty())) else {
        return Ok(Resolved::Err("The League client did not give your Riot ID (Name#TAG) yet.".into()));
    };
    let riot_id = format!("{name}#{tag}");
    let from_client = to_platform(manual_platform).or_else(|| to_platform(a.platform.as_deref()));
    let Some(puuid) = riot.account_by_riot_id(name, tag, account_host(from_client.as_deref()), signal).await?.filter(|p| !p.is_empty()) else {
        return Ok(Resolved::Err(format!("Riot doesn't know the account {riot_id}.")));
    };
    let mut platform = from_client;
    if platform.is_none() {
        platform = match riot.account_region(&puuid, signal).await {
            Ok(r) => to_platform(r.as_deref()),
            Err(_) => None,
        };
    }
    match platform {
        Some(platform) => Ok(Resolved::Ok { puuid, platform, riot_id }),
        None => Ok(Resolved::Err(format!("Could not tell which server {riot_id} plays on: choose it in Settings → Your account."))),
    }
}
