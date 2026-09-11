//! Configuration: one TOML file, defaults plus per-account overrides.
//!
//! Every setting lives under `[defaults.<action>]` and may be overridden per account
//! under `[accounts.<name>.<action>]`. The merge is a deep merge of the raw TOML
//! tables performed *before* deserialization, so an override table only needs the keys
//! it actually changes and every new setting gets the behaviour for free.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Resolved configuration: defaults already merged into every account.
#[derive(Debug, Clone)]
pub struct Config {
    pub general: General,
    pub hive: Hive,
    pub terracore: Terracore,
    pub wallet: WalletConfig,
    pub blacklist: Blacklist,
    pub web: Web,
    pub accounts: Vec<Account>,
    /// Where this was loaded from, for error messages and for the future web UI.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct General {
    /// Seconds between cycles. The old bot slept four hours; attacks regenerate
    /// faster than that, so the default is tighter and the bot simply finds nothing
    /// to do on an early wake-up.
    pub cycle_interval_secs: u64,
    /// Pause between accounts, to avoid hammering the API from one IP.
    pub account_delay_secs: u64,
    /// Log what would be broadcast without broadcasting it.
    pub dry_run: bool,
    /// Skip a cycle when the game's own transaction queue is longer than this.
    /// 0 disables the check. The queue is what the website shows as pending work.
    pub max_transaction_queue: u64,
}

impl Default for General {
    fn default() -> Self {
        Self {
            cycle_interval_secs: 900,
            account_delay_secs: 3,
            dry_run: false,
            max_transaction_queue: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Hive {
    /// Tried in order; the client fails over on error and tracks node health.
    pub nodes: Vec<String>,
    pub timeout_secs: u64,
    /// Transaction expiry handed to hived. One minute is plenty for a bot.
    pub expiration_secs: u32,
    /// How long a cached block reference may be reused before it is refreshed.
    /// TaPoS stays valid far longer than this; the cap is about staleness, not
    /// correctness.
    pub tapos_max_age_secs: u64,
}

impl Default for Hive {
    fn default() -> Self {
        Self {
            nodes: vec![
                "https://api.hive.blog".into(),
                "https://api.openhive.network".into(),
                "https://hive-api.arcange.eu".into(),
                "https://hived.emre.sh".into(),
            ],
            timeout_secs: 15,
            expiration_secs: 60,
            tapos_max_age_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Terracore {
    pub api: String,
    pub timeout_secs: u64,
    /// Retries per request, matching the website's own client.
    pub retries: u32,
}

impl Default for Terracore {
    fn default() -> Self {
        Self {
            api: "https://api.terracoregame.com".into(),
            timeout_secs: 20,
            retries: 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WalletConfig {
    pub path: String,
    /// Environment variable holding the passphrase. Empty means prompt on a tty.
    pub passphrase_env: String,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            path: "~/.config/terracore-bot/wallet.json".into(),
            passphrase_env: "TERRACORE_WALLET_PASSPHRASE".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Blacklist {
    /// Never attacked, whatever the numbers say.
    pub accounts: Vec<String>,
    /// Fetched each cycle and merged with the above. A fetch failure is logged and
    /// the previous list is kept -- it must never open the door to attacking a
    /// protected account.
    pub urls: Vec<String>,
    /// Accounts configured in this file never attack each other.
    pub skip_own_accounts: bool,
}

impl Default for Blacklist {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            urls: Vec::new(),
            skip_own_accounts: true,
        }
    }
}

/// The local control panel. Off unless asked for.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Web {
    pub enabled: bool,
    /// Loopback by default. Anything else exposes a panel that can change what your
    /// accounts do, so put a TLS reverse proxy in front of it.
    pub bind: String,
    /// How long a Keychain login lasts.
    pub session_hours: u64,
    /// Which Hive account may log in, and as what.
    ///
    /// `admin` changes anything and controls the bot; `operator` may edit only its
    /// own account's settings; `viewer` reads and changes nothing. An account not
    /// listed here cannot log in at all, however good its signature.
    pub access: BTreeMap<String, WebRole>,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1:8787".into(),
            session_hours: 12,
            access: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum WebRole {
    Viewer,
    Operator,
    Admin,
}

impl WebRole {
    pub fn as_str(self) -> &'static str {
        match self {
            WebRole::Viewer => "viewer",
            WebRole::Operator => "operator",
            WebRole::Admin => "admin",
        }
    }

    /// Whether this role may see, or change, `account`. An operator is scoped to its
    /// own account and nothing else; that scoping is the whole point of the role.
    pub fn may_read(self, session_account: &str, account: &str) -> bool {
        match self {
            WebRole::Admin | WebRole::Viewer => true,
            WebRole::Operator => session_account.eq_ignore_ascii_case(account),
        }
    }

    pub fn may_write(self, session_account: &str, account: &str) -> bool {
        match self {
            WebRole::Admin => true,
            WebRole::Operator => session_account.eq_ignore_ascii_case(account),
            WebRole::Viewer => false,
        }
    }

    /// Controlling the bot itself, and reading the cross-account log, is admin only.
    pub fn is_admin(self) -> bool {
        self == WebRole::Admin
    }
}

/// One configured account with its fully merged settings.
#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub enabled: bool,
    pub settings: Settings,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub attack: AttackSettings,
    pub claim: ClaimSettings,
    pub quest: QuestSettings,
    pub boss: BossSettings,
    pub spend: SpendSettings,
}

impl Settings {
    /// The actions this account will attempt, in the order a cycle runs them.
    ///
    /// Stated once. Start-up and `status` both report this, and two copies of the
    /// list would eventually disagree about what the bot actually does.
    pub fn enabled_actions(&self) -> Vec<&'static str> {
        let mut actions = Vec::new();
        if self.attack.enabled {
            actions.push("attack");
        }
        if self.claim.enabled {
            actions.push("claim");
        }
        if self.quest.enabled && self.quest.collect {
            actions.push("quests");
        }
        if self.spend.enabled {
            actions.push("spend");
        }
        if self.boss.enabled {
            actions.push("boss");
        }
        actions
    }

    /// Whether anything enabled here moves Hive-Engine tokens, and so cannot run
    /// without an active key in the wallet.
    pub fn needs_active_key(&self) -> bool {
        self.boss.enabled || self.spend.enabled
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct AttackSettings {
    pub enabled: bool,
    /// Do not start attacking below this many available attacks.
    pub min_attacks: u32,
    /// Attacks per cycle. 0 means "until they run out".
    pub max_per_cycle: u32,
    pub delay_secs: u64,
    /// Skip targets that dodge more than this often. Dodged attacks still spend
    /// the attack.
    pub max_enemy_dodge: f64,
    /// Ignore targets holding less than this much scrap.
    pub min_target_scrap: f64,
    /// Require `damage - defense` to be at least this. The game only requires a
    /// margin above zero; a buffer absorbs a target upgrading between the battle
    /// list and the transaction landing.
    pub min_damage_margin: f64,
    /// Fetch `/battle_focus` and ignore the defense rule when a focus charge is
    /// held. Each attack on an over-defended target burns one charge.
    pub use_focus: bool,
    /// Battle list page size requested from the API.
    pub candidate_limit: u32,
}

impl Default for AttackSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            min_attacks: 1,
            max_per_cycle: 0,
            delay_secs: 20,
            max_enemy_dodge: 60.0,
            min_target_scrap: 1.0,
            min_damage_margin: 0.0,
            use_focus: false,
            candidate_limit: 100,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ClaimSettings {
    pub enabled: bool,
    /// Do not spend a claim on less scrap than this.
    pub min_scrap: f64,
    /// Keep this many claims in reserve; attacking needs at least one.
    pub min_claims: u32,
    /// Claim before attacking when the stash is full, since a full stash makes
    /// every attack pointless -- looted scrap would have nowhere to go.
    pub claim_when_stash_full: bool,
    /// The website enforces 30s between claims client-side. Honoured here too.
    pub cooldown_secs: u64,
}

impl Default for ClaimSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            min_scrap: 0.1,
            min_claims: 1,
            claim_when_stash_full: true,
            cooldown_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct QuestSettings {
    pub enabled: bool,
    /// Collect finished missions. Free: it only harvests rewards.
    pub collect: bool,
    pub delay_secs: u64,
}

impl Default for QuestSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            collect: true,
            delay_secs: 5,
        }
    }
}

/// Boss fights burn FLUX through Hive-Engine, so they need an **active** key.
/// Disabled unless both this flag and an active key in the wallet say otherwise.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BossSettings {
    pub enabled: bool,
    /// Empty means every unlocked planet. Names are the planet names as the API
    /// reports them, e.g. "Terracore", "Oceana".
    pub planets: Vec<String>,
    pub skip_planets: Vec<String>,
    /// Never fight a planet whose entry price is above this.
    pub max_flux_per_fight: f64,
    /// Stop once the FLUX balance would fall below this.
    pub min_flux_reserve: f64,
    /// Fights per cycle across all planets. 0 means every eligible planet.
    pub max_per_cycle: u32,
    /// `highest_level` fights the hardest planet first (better drops),
    /// `cheapest` the lowest FLUX cost first, `listed` follows `planets`.
    pub order: BossOrder,
    pub delay_secs: u64,
}

impl Default for BossSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            planets: Vec::new(),
            skip_planets: Vec::new(),
            max_flux_per_fight: 5.0,
            min_flux_reserve: 0.0,
            max_per_cycle: 0,
            order: BossOrder::HighestLevel,
            delay_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BossOrder {
    HighestLevel,
    Cheapest,
    Listed,
}

/// How surplus SCRAP is divided between the things it can be spent on.
///
/// Not a waterfall: a waterfall pours everything into whichever goal comes first and
/// the others never move. Each cycle the spendable balance is split by `weight`, so
/// all three advance together, and a goal that has reached its ceiling hands its
/// share back to the rest rather than wasting it.
///
/// The one thing outside the rotation is `min_stash_hours`. A full stash stops the
/// bot attacking at all, so it is a need rather than a preference and is fixed first.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SpendSettings {
    pub enabled: bool,
    /// Never spend liquid SCRAP below this.
    pub min_scrap_reserve: f64,
    /// Stake enough to hold this many hours of mining before anything else is
    /// considered. 0 disables the check.
    pub min_stash_hours: f64,
    pub engineering: EngineeringGoal,
    pub favor: FavorGoal,
    pub stake: StakeGoal,
    /// Damage and defense, bought only on evidence -- see `DamageGoal`.
    pub damage: DamageGoal,
    pub delay_secs: u64,
    /// Purchases per cycle across all goals. 0 means no cap.
    pub max_per_cycle: u32,
}

impl Default for SpendSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            min_scrap_reserve: 0.0,
            min_stash_hours: 12.0,
            engineering: EngineeringGoal::default(),
            favor: FavorGoal::default(),
            stake: StakeGoal::default(),
            damage: DamageGoal::default(),
            delay_secs: 10,
            max_per_cycle: 0,
        }
    }
}

/// Engineering is the only goal that compounds: it raises mining income, which pays
/// for everything else. Below the game's 333 softcap the payback in days is very
/// close to the current level, so the ceiling is expressed in days.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct EngineeringGoal {
    pub enabled: bool,
    pub weight: f64,
    /// Stop once a point takes longer than this to mine back its own cost.
    pub max_payback_days: f64,
    /// Hard ceiling on the stat. 0 disables it.
    pub max_level: f64,
}

impl Default for EngineeringGoal {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: 3.0,
            max_payback_days: 60.0,
            max_level: 0.0,
        }
    }
}

/// Favor buys critical-hit chance and is burned for good. Its cost is flat within a
/// band and doubles at every band edge, so a ceiling on the *marginal* price stops
/// exactly at the next cliff -- which a target percentage cannot do, because the
/// cliff moves as the account grows.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct FavorGoal {
    pub enabled: bool,
    pub weight: f64,
    /// Stop once one more percent of crit costs more than this.
    pub max_scrap_per_crit_point: f64,
    /// Hard ceiling on crit. 0 disables it.
    pub max_crit: f64,
}

impl Default for FavorGoal {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: 1.0,
            max_scrap_per_crit_point: 100_000.0,
            max_crit: 0.0,
        }
    }
}

/// Staking is not spending: the SCRAP stays yours. It raises dodge, luck and the
/// stash ceiling, so it is the natural home for anything the other goals cannot
/// justify -- at the cost of the unstaking cooldown.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct StakeGoal {
    pub enabled: bool,
    pub weight: f64,
    /// Stop chasing dodge once a point of it costs more than this. Staking past
    /// that still buys stash, which `absorb_surplus` decides on.
    pub max_scrap_per_dodge_point: f64,
    /// Put whatever the other goals could not use into stake. Off by default: the
    /// leftover otherwise stays liquid and accumulates, which is what lets an
    /// expensive engineering point eventually become affordable.
    pub absorb_surplus: bool,
    /// Hard ceiling on total stake. 0 disables it.
    pub max_stake: f64,
}

impl Default for StakeGoal {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: 2.0,
            max_scrap_per_dodge_point: 50_000.0,
            absorb_surplus: false,
            max_stake: 0.0,
        }
    }
}

/// Damage widens the set of players you can attack and defense narrows the set that
/// can attack you. Neither is bought on a schedule: damage is bought only when the
/// battle board says targets are actually out of reach, which the bot already counts.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DamageGoal {
    pub enabled: bool,
    pub weight: f64,
    /// Buy damage only while at least this percentage of the board is unreachable.
    pub min_unreachable_percent: f64,
    pub max_level: f64,
    /// Defense is never bought on evidence the bot has, so it is opt-in and capped.
    pub defense_enabled: bool,
    pub defense_weight: f64,
    pub max_defense: f64,
}

impl Default for DamageGoal {
    fn default() -> Self {
        Self {
            enabled: false,
            weight: 1.0,
            min_unreachable_percent: 20.0,
            max_level: 0.0,
            defense_enabled: false,
            defense_weight: 1.0,
            max_defense: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stat {
    Engineering,
    Damage,
    Defense,
}

impl Stat {
    pub fn as_str(self) -> &'static str {
        match self {
            Stat::Engineering => "engineering",
            Stat::Damage => "damage",
            Stat::Defense => "defense",
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg =
            Self::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.path = path.to_path_buf();
        Ok(cfg)
    }

    pub fn from_str(text: &str) -> Result<Self> {
        let root: toml::Value = toml::from_str(text)?;
        let table = root
            .as_table()
            .context("the config must be a table at the top level")?;

        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "general"
                    | "hive"
                    | "terracore"
                    | "wallet"
                    | "blacklist"
                    | "web"
                    | "defaults"
                    | "accounts"
            ) {
                bail!("unknown top-level section `{key}`");
            }
        }

        let general: General = section(table, "general")?;
        let hive: Hive = section(table, "hive")?;
        let terracore: Terracore = section(table, "terracore")?;
        let wallet: WalletConfig = section(table, "wallet")?;
        let blacklist: Blacklist = section(table, "blacklist")?;
        let web: Web = section(table, "web")?;
        if web.enabled && web.access.is_empty() {
            bail!("[web] is enabled but [web.access] names nobody, so nobody could log in");
        }

        // Validated on its own first, so a typo in `[defaults]` is reported against
        // `[defaults]` rather than against whichever account inherited it.
        let defaults_value = table
            .get("defaults")
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
        let _: Settings = defaults_value
            .clone()
            .try_into()
            .context("in section [defaults]")?;

        let accounts_table = match table.get("accounts") {
            Some(toml::Value::Table(t)) => t.clone(),
            Some(_) => bail!("`accounts` must be a table of account names"),
            None => toml::map::Map::new(),
        };
        if accounts_table.is_empty() {
            bail!("no accounts configured -- add at least one `[accounts.<name>]` section");
        }

        // BTreeMap: a stable, name-sorted order, so the log of a cycle reads the
        // same every time regardless of how the file happens to be arranged.
        let mut by_name: BTreeMap<String, Account> = BTreeMap::new();
        for (name, value) in accounts_table {
            let mut account_table = match value {
                toml::Value::Table(t) => t,
                _ => bail!("[accounts.{name}] must be a table"),
            };
            // Reserved keys live on the account, not in the merged settings.
            let enabled = match account_table.remove("enabled") {
                Some(toml::Value::Boolean(b)) => b,
                Some(_) => bail!("[accounts.{name}] `enabled` must be a boolean"),
                None => true,
            };

            let merged = deep_merge(defaults_value.clone(), toml::Value::Table(account_table));
            let settings: Settings = merged
                .try_into()
                .with_context(|| format!("in section [accounts.{name}]"))?;

            validate_account(&name, &settings)?;
            by_name.insert(
                name.clone(),
                Account {
                    name,
                    enabled,
                    settings,
                },
            );
        }

        Ok(Config {
            general,
            hive,
            terracore,
            wallet,
            blacklist,
            web,
            accounts: by_name.into_values().collect(),
            path: PathBuf::new(),
        })
    }

    /// Every configured account name, enabled or not. Used to keep sibling accounts
    /// out of each other's target lists.
    pub fn account_names(&self) -> Vec<String> {
        self.accounts.iter().map(|a| a.name.clone()).collect()
    }

    /// The settings an account with no overrides at all ends up with. The web UI
    /// hands back effective values; this is what they are diffed against so the file
    /// keeps only what actually differs.
    pub fn default_settings(path_text: &str) -> Result<Settings> {
        let root: toml::Value = toml::from_str(path_text)?;
        let defaults = root
            .as_table()
            .and_then(|t| t.get("defaults"))
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
        defaults.try_into().context("in section [defaults]")
    }

    pub fn wallet_path(&self) -> PathBuf {
        expand_tilde(&self.wallet.path)
    }
}

fn validate_account(name: &str, s: &Settings) -> Result<()> {
    if s.attack.max_enemy_dodge < 0.0 || s.attack.max_enemy_dodge > 100.0 {
        bail!("[accounts.{name}.attack] max_enemy_dodge must be between 0 and 100");
    }
    if s.attack.candidate_limit == 0 {
        bail!("[accounts.{name}.attack] candidate_limit must be at least 1");
    }
    if s.spend.enabled {
        let w = &s.spend;
        let any = (w.engineering.enabled && w.engineering.weight > 0.0)
            || (w.favor.enabled && w.favor.weight > 0.0)
            || (w.stake.enabled && w.stake.weight > 0.0)
            || (w.damage.enabled && w.damage.weight > 0.0);
        if !any {
            bail!("[accounts.{name}.spend] is enabled but every goal is off or has weight 0");
        }
        if w.min_stash_hours < 0.0 {
            bail!("[accounts.{name}.spend] min_stash_hours cannot be negative");
        }
    }
    if s.boss.order == BossOrder::Listed && s.boss.enabled && s.boss.planets.is_empty() {
        bail!("[accounts.{name}.boss] order = \"listed\" needs a non-empty `planets` list");
    }
    Ok(())
}

fn section<T: serde::de::DeserializeOwned + Default>(
    table: &toml::map::Map<String, toml::Value>,
    name: &str,
) -> Result<T> {
    match table.get(name) {
        Some(value) => value
            .clone()
            .try_into()
            .with_context(|| format!("in section [{name}]")),
        None => Ok(T::default()),
    }
}

/// Recursively overlay `over` onto `base`. Tables merge key by key; every other
/// value, arrays included, replaces wholesale -- a per-account `stats = ["damage"]`
/// means exactly that list, not that list appended to the default.
fn deep_merge(base: toml::Value, over: toml::Value) -> toml::Value {
    match (base, over) {
        (toml::Value::Table(mut b), toml::Value::Table(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            toml::Value::Table(b)
        }
        (_, over) => over,
    }
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[defaults.attack]
max_enemy_dodge = 40.0
delay_secs = 11

[accounts.alice]

[accounts.bob]
enabled = false
[accounts.bob.attack]
max_enemy_dodge = 5.0
"#;

    #[test]
    fn defaults_reach_every_account_and_overrides_win() {
        let cfg = Config::from_str(BASE).unwrap();
        let alice = &cfg.accounts[0];
        let bob = &cfg.accounts[1];

        assert_eq!(alice.name, "alice");
        assert!(alice.enabled);
        assert_eq!(alice.settings.attack.max_enemy_dodge, 40.0);
        assert_eq!(alice.settings.attack.delay_secs, 11);

        assert!(!bob.enabled);
        assert_eq!(bob.settings.attack.max_enemy_dodge, 5.0);
        // Untouched by the override, so it still comes from [defaults].
        assert_eq!(bob.settings.attack.delay_secs, 11);
        // Untouched by either, so it comes from the compiled-in default.
        assert!(bob.settings.claim.enabled);
    }

    #[test]
    fn active_key_actions_are_off_unless_asked_for() {
        let cfg = Config::from_str("[accounts.alice]\n").unwrap();
        assert!(!cfg.accounts[0].settings.boss.enabled);
        assert!(!cfg.accounts[0].settings.spend.enabled);
    }

    #[test]
    fn a_typo_is_an_error_rather_than_a_silently_ignored_setting() {
        let err = Config::from_str("[defaults.attack]\nmax_enemy_dodg = 40\n[accounts.a]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("defaults"), "{err}");

        let err = Config::from_str("[genral]\n[accounts.a]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown top-level section"), "{err}");
    }

    /// The shipped example states a value for every setting and calls them the
    /// defaults. If a compiled default changes and the file is not updated, the file
    /// is quietly lying to whoever copies it -- which is the whole failure mode a
    /// second copy of anything invites.
    #[test]
    fn the_example_config_states_the_actual_compiled_defaults() {
        let example = include_str!("../config.example.toml");
        let documented = Config::default_settings(example).expect("the shipped example must parse");

        let as_toml = |s: &Settings| toml::Value::try_from(s).unwrap();
        assert_eq!(
            as_toml(&documented),
            as_toml(&Settings::default()),
            "config.example.toml no longer matches the compiled defaults"
        );

        // And it is a config the bot would actually accept.
        let loadable = example.replace("[accounts.youraccount]", "[accounts.probe]");
        Config::from_str(&loadable).expect("the shipped example must load");
    }

    #[test]
    fn a_config_with_no_accounts_is_refused() {
        assert!(Config::from_str("[general]\ndry_run = true\n").is_err());
    }

    #[test]
    fn the_enabled_action_list_reflects_the_settings() {
        let mut s = Settings::default();
        // The defaults: the three a posting key can do, and neither of the two that
        // spend tokens.
        assert_eq!(s.enabled_actions(), ["attack", "claim", "quests"]);
        assert!(!s.needs_active_key());

        s.quest.collect = false;
        assert_eq!(s.enabled_actions(), ["attack", "claim"]);

        s.boss.enabled = true;
        assert!(s.needs_active_key());
        assert!(s.enabled_actions().contains(&"boss"));

        s.spend.enabled = true;
        s.attack.enabled = false;
        s.claim.enabled = false;
        assert_eq!(s.enabled_actions(), ["spend", "boss"]);

        // Each of the two token-spending actions is enough on its own.
        let mut only_spend = Settings::default();
        only_spend.spend.enabled = true;
        assert!(only_spend.needs_active_key());
        let mut only_boss = Settings::default();
        only_boss.boss.enabled = true;
        assert!(only_boss.needs_active_key());
    }

    #[test]
    fn an_out_of_range_dodge_cap_is_refused() {
        let err = Config::from_str("[accounts.a.attack]\nmax_enemy_dodge = 140\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_enemy_dodge"), "{err}");
    }
}
