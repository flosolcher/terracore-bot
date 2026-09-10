//! The Terracore REST API, and the shapes it returns.
//!
//! Every model is tolerant: unknown fields are ignored and missing ones default, so a
//! server-side addition cannot take the bot down mid-cycle. The field names and the
//! meaning of each one were read out of the game's own client bundle.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Milliseconds. The API speaks epoch-millis throughout, so the bot does too rather
/// than converting back and forth at every comparison.
pub type Millis = i64;

pub fn now_ms() -> Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

// The models below mirror the API's shape rather than only the handful of fields
// used today: a field that is modelled and named is a field the next feature does
// not have to rediscover, and `deny_unknown_fields` is deliberately absent so a
// server-side addition cannot break a running bot.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Stats {
    #[serde(default)]
    pub damage: f64,
    #[serde(default)]
    pub defense: f64,
    #[serde(default)]
    pub engineering: f64,
    #[serde(default)]
    pub dodge: f64,
    #[serde(default)]
    pub crit: f64,
    #[serde(default)]
    pub luck: f64,
}

/// Only the consumables the bot reasons about. The rest of the object is ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Consumables {
    /// How many protection charges are held. Presence alone does not mean protected;
    /// the timestamps decide.
    #[serde(default)]
    pub protection: f64,
    #[serde(default)]
    pub protection_times: Vec<f64>,
    /// Focus charges. Each one buys a single attack against a target whose defense
    /// exceeds your damage.
    #[serde(default)]
    pub focus: f64,
}

impl Consumables {
    /// Milliseconds of attack immunity left, mirroring the website's
    /// `getRemainingProtectionTime`: only the most recent charge counts, and it runs
    /// for 24 hours.
    pub fn protection_remaining_ms(&self, now: Millis) -> Millis {
        if self.protection <= 0.0 {
            return 0;
        }
        match self.protection_times.last() {
            Some(&last) => {
                let ends = last as Millis + 24 * 60 * 60 * 1000;
                (ends - now).max(0)
            }
            None => 0,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Item {
    #[serde(default)]
    pub item_number: Option<i64>,
    #[serde(default)]
    pub item_id: Option<i64>,
    #[serde(default)]
    pub item_equipped: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Items {
    #[serde(default)]
    pub avatar: Item,
    #[serde(default)]
    pub weapon: Item,
    #[serde(default)]
    pub armor: Item,
    #[serde(default)]
    pub ship: Item,
    #[serde(default)]
    pub special: Item,
}

/// The bot's own account state.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Player {
    #[serde(default)]
    pub username: String,
    /// Unclaimed scrap sitting in the stash. Claiming moves it to the Hive-Engine
    /// balance; it is *not* spendable where it is.
    #[serde(default)]
    pub scrap: f64,
    #[serde(default)]
    pub attacks: f64,
    #[serde(rename = "maxAttacks", default)]
    pub max_attacks: Option<f64>,
    #[serde(default)]
    pub claims: f64,
    #[serde(default)]
    pub level: f64,
    #[serde(default)]
    pub experience: f64,
    /// Base stats, before items. These, not the effective `stats`, are what an
    /// upgrade's price is computed from.
    #[serde(default)]
    pub damage: f64,
    #[serde(default)]
    pub defense: f64,
    #[serde(default)]
    pub engineering: f64,
    /// Effective stats including equipped items and buffs. These are what a battle
    /// is decided on.
    #[serde(default)]
    pub stats: Stats,
    /// Liquid SCRAP in the Hive-Engine wallet -- what upgrades are paid from.
    #[serde(rename = "hiveEngineScrap", default)]
    pub hive_engine_scrap: f64,
    /// Staked SCRAP. Also the stash ceiling: the stash holds `stake + 1`.
    #[serde(rename = "hiveEngineStake", default)]
    pub hive_engine_stake: f64,
    #[serde(default)]
    pub flux: f64,
    #[serde(default)]
    pub consumables: Consumables,
    #[serde(default)]
    pub items: Items,
    #[serde(rename = "lastBattle", default)]
    pub last_battle: Millis,
    /// Present instead of the rest of the object when the account is unknown.
    #[serde(default)]
    pub error: Option<String>,
}

impl Player {
    /// The website blocks attacking once the stash is full, because looted scrap
    /// would have nowhere to land. The ceiling is the staked balance plus one.
    pub fn stash_is_full(&self) -> bool {
        self.scrap >= self.hive_engine_stake + 1.0
    }

    pub fn stash_capacity(&self) -> f64 {
        self.hive_engine_stake + 1.0
    }

    pub fn focus_charges(&self) -> u32 {
        self.consumables.focus.max(0.0) as u32
    }
}

/// One row of the battle board: a candidate to attack.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Target {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub scrap: f64,
    /// Top-level base stats. The website reads `stats.*` first and falls back here.
    #[serde(default)]
    pub defense: f64,
    #[serde(default)]
    pub dodge: f64,
    #[serde(default)]
    pub stats: Stats,
    #[serde(rename = "registrationTime", default)]
    pub registration_time: Millis,
    #[serde(rename = "lastBattle", default)]
    pub last_battle: Millis,
    #[serde(default)]
    pub consumables: Consumables,
}

impl Target {
    /// Effective defense, preferring the computed stats exactly as the client does.
    pub fn defense(&self) -> f64 {
        if self.stats.defense != 0.0 {
            self.stats.defense
        } else {
            self.defense
        }
    }

    pub fn dodge(&self) -> f64 {
        if self.stats.dodge != 0.0 {
            self.stats.dodge
        } else {
            self.dodge
        }
    }

    /// Scrap discounted by the chance of being dodged -- the website's own ranking.
    /// A dodged attack still spends the attack, so this is what an attack is worth.
    pub fn expected_scrap(&self) -> f64 {
        self.scrap - self.scrap * (self.dodge() / 100.0)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BattleResponse {
    #[serde(default)]
    players: Vec<Target>,
}

/// A planet and its boss, as carried in the player's `boss_data`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Planet {
    #[serde(default)]
    pub name: String,
    /// Player level required to travel here.
    #[serde(default)]
    pub level: f64,
    /// FLUX burned per fight.
    #[serde(default)]
    pub flux: f64,
    #[serde(rename = "lastBattle", default)]
    pub last_battle: Millis,
}

/// Boss cooldown, from the client: four hours after the last fight on that planet.
pub const BOSS_COOLDOWN_MS: Millis = 4 * 60 * 60 * 1000;

impl Planet {
    pub fn next_battle(&self) -> Millis {
        self.last_battle + BOSS_COOLDOWN_MS
    }

    pub fn ready(&self, now: Millis) -> bool {
        now >= self.next_battle()
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlanetsResponse {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub level: f64,
    #[serde(default)]
    pub flux: f64,
    #[serde(default)]
    pub items: Items,
    #[serde(default)]
    pub boss_data: Vec<Planet>,
}

impl PlanetsResponse {
    pub fn has_ship(&self) -> bool {
        self.items.ship.item_number.is_some_and(|n| n >= 0)
    }
}

/// An accepted mission. `completes_at` is when its rewards become collectable.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Quest {
    #[serde(rename = "_id", default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub quest_type: String,
    #[serde(default)]
    pub tier: f64,
    #[serde(default)]
    pub completes_at: Millis,
    #[serde(default)]
    pub collected: bool,
}

impl Quest {
    /// The client's rule: finished, and not already harvested.
    pub fn collectable(&self, now: Millis) -> bool {
        !self.collected && self.completes_at > 0 && self.completes_at <= now && !self.id.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Api {
    base: String,
    agent: ureq::Agent,
    retries: u32,
}

impl Api {
    pub fn new(base: &str, timeout: Duration, retries: u32) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(timeout)
            .user_agent(concat!("terracore-bot/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            base: base.trim_end_matches('/').to_string(),
            agent,
            retries,
        }
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        let mut last: Option<anyhow::Error> = None;
        // `retries` is the number of *extra* attempts, matching the website's client.
        for attempt in 0..=self.retries {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(400 * attempt as u64));
            }
            let mut request = self.agent.get(&url);
            for (k, v) in query {
                request = request.query(k, v);
            }
            match request.call() {
                Ok(response) => {
                    return response
                        .into_json::<T>()
                        .with_context(|| format!("decoding response from {url}"))
                }
                // A 404 is an answer, not a transport failure: retrying cannot change
                // it, and the caller wants to hear about it now.
                Err(ureq::Error::Status(404, _)) => bail!("{url} returned 404"),
                Err(e) => last = Some(anyhow::Error::new(e).context(format!("requesting {url}"))),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("requesting {url} failed")))
    }

    pub fn player(&self, name: &str) -> Result<Player> {
        let player: Player = self.get(&format!("/player/{name}"), &[])?;
        if let Some(error) = &player.error {
            bail!("the game reports `{error}` for {name}");
        }
        if player.username.is_empty() {
            bail!("the game returned no username for {name}");
        }
        Ok(player)
    }

    /// The battle board, already filtered server-side to defenses below `max_defense`.
    ///
    /// With a focus charge held the client asks `/battle_focus` instead, which is not
    /// capped by defense at all -- a focus charge buys an attack on anyone.
    pub fn battles(&self, max_defense: f64, limit: u32, offset: u32, focus: bool) -> Result<Vec<Target>> {
        let path = if focus { "/battle_focus" } else { "/battle" };
        let mut query = vec![
            ("limit", limit.to_string()),
            ("offset", offset.to_string()),
        ];
        // The website omits the parameter entirely when it would be zero.
        if max_defense != 0.0 {
            query.push(("maxDefense", format_number(max_defense)));
        }
        let response: BattleResponse = self.get(path, &query)?;
        Ok(response.players)
    }

    pub fn planets(&self, name: &str) -> Result<PlanetsResponse> {
        self.get(&format!("/planets_new/{name}"), &[])
    }

    pub fn quests(&self, name: &str) -> Result<Vec<Quest>> {
        // An account that has never taken a mission gets `[]`; one whose quest record
        // is missing entirely gets an object with an error. Both mean "nothing to do".
        match self.get::<serde_json::Value>(&format!("/quests/{name}"), &[]) {
            Ok(serde_json::Value::Array(items)) => Ok(items
                .into_iter()
                .filter_map(|v| serde_json::from_value(v).ok())
                .collect()),
            Ok(_) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// How many game transactions are queued for processing. A long queue means the
    /// game is behind and a burst of attacks would simply pile on.
    pub fn transaction_queue(&self) -> Result<u64> {
        #[derive(Deserialize, Default)]
        struct Queue {
            #[serde(default)]
            transactions: f64,
        }
        let queue: Queue = self.get("/transactions", &[])?;
        Ok(queue.transactions.max(0.0) as u64)
    }
}

/// Render a float the way JavaScript's `String(x)` would: no trailing `.0`, no
/// exponent for the magnitudes this game deals in. Amounts sent to Hive-Engine are
/// compared as text by the contract, so this is not cosmetic.
pub fn format_number(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        // Round to Hive-Engine's eight decimals, then let Rust pick the shortest
        // representation that round-trips.
        let rounded = (value * 1e8).round() / 1e8;
        format!("{rounded}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_render_like_the_website_does() {
        assert_eq!(format_number(269361.0), "269361");
        assert_eq!(format_number(108570.25), "108570.25");
        assert_eq!(format_number(0.1), "0.1");
        assert_eq!(format_number(2.0), "2");
    }

    #[test]
    fn protection_counts_only_the_most_recent_charge() {
        let now = 1_000_000_000_000;
        let day = 24 * 60 * 60 * 1000;
        let c = Consumables {
            protection: 1.0,
            // An old charge and a fresh one: only the last is looked at.
            protection_times: vec![(now - 3 * day) as f64, (now - day / 2) as f64],
            focus: 0.0,
        };
        assert_eq!(c.protection_remaining_ms(now), day / 2);

        let expired = Consumables {
            protection: 1.0,
            protection_times: vec![(now - 2 * day) as f64],
            focus: 0.0,
        };
        assert_eq!(expired.protection_remaining_ms(now), 0);

        // A charge held but never used protects nobody.
        let unused = Consumables {
            protection: 0.0,
            protection_times: vec![(now - 1000) as f64],
            focus: 0.0,
        };
        assert_eq!(unused.protection_remaining_ms(now), 0);
    }

    #[test]
    fn a_target_prefers_computed_stats_over_the_flat_fields() {
        let t = Target {
            defense: 100.0,
            dodge: 10.0,
            stats: Stats {
                defense: 250.0,
                dodge: 40.0,
                ..Default::default()
            },
            scrap: 200.0,
            ..Default::default()
        };
        assert_eq!(t.defense(), 250.0);
        assert_eq!(t.dodge(), 40.0);
        assert_eq!(t.expected_scrap(), 120.0);
    }

    #[test]
    fn the_stash_ceiling_is_the_stake_plus_one() {
        let p = Player {
            scrap: 51.0,
            hive_engine_stake: 50.0,
            ..Default::default()
        };
        assert!(p.stash_is_full());
        let p = Player {
            scrap: 50.9,
            hive_engine_stake: 50.0,
            ..Default::default()
        };
        assert!(!p.stash_is_full());
    }

    #[test]
    fn a_quest_is_collectable_once_and_only_once_it_has_finished() {
        let now = 1_000;
        let done = Quest {
            id: "x".into(),
            completes_at: 999,
            ..Default::default()
        };
        assert!(done.collectable(now));

        let running = Quest {
            id: "x".into(),
            completes_at: 1_001,
            ..Default::default()
        };
        assert!(!running.collectable(now));

        let harvested = Quest {
            id: "x".into(),
            completes_at: 999,
            collected: true,
            ..Default::default()
        };
        assert!(!harvested.collectable(now));
    }
}
