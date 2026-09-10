//! One cycle over every configured account, and the loop around it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::actions::{log_failure, Outcome, Runner};
use crate::api::Api;
use crate::config::Config;
use crate::hive::Broadcaster;
use crate::keys::KeyStore;

pub struct Bot {
    config: Config,
    api: Api,
    hive: Broadcaster,
    keys: KeyStore,
    blacklist: HashSet<String>,
    /// When each account last claimed, so the website's 30-second claim cooldown is
    /// honoured across the pre-claim and the end-of-cycle claim.
    last_claim: HashMap<String, Instant>,
    stop: Arc<AtomicBool>,
}

impl Bot {
    pub fn new(config: Config, stop: Arc<AtomicBool>) -> Result<Self> {
        let api = Api::new(
            &config.terracore.api,
            Duration::from_secs(config.terracore.timeout_secs),
            config.terracore.retries,
        );
        let hive = Broadcaster::new(
            config.hive.nodes.clone(),
            Duration::from_secs(config.hive.timeout_secs),
            config.hive.expiration_secs,
            Duration::from_secs(config.hive.tapos_max_age_secs),
            config.general.dry_run,
        )?;
        let keys = KeyStore::open(&config.wallet_path(), &config.wallet.passphrase_env)?;

        let mut bot = Self {
            blacklist: HashSet::new(),
            last_claim: HashMap::new(),
            config,
            api,
            hive,
            keys,
            stop,
        };
        bot.rebuild_blacklist();
        Ok(bot)
    }

    /// Check every configured account resolves to a key and print what each one can
    /// do. Run once at startup so a missing key is an error before the first cycle,
    /// not a surprise four hours in.
    pub fn preflight(&self) -> Result<()> {
        self.hive.verify_chain()?;
        for account in &self.config.accounts {
            if !account.enabled {
                info!(account = %account.name, "disabled in the config; skipping");
                continue;
            }
            let keys = self.keys.for_account(&account.name)?;
            let s = &account.settings;
            let mut enabled: Vec<&str> = Vec::new();
            if s.attack.enabled {
                enabled.push("attack");
            }
            if s.claim.enabled {
                enabled.push("claim");
            }
            if s.quest.enabled && s.quest.collect {
                enabled.push("quests");
            }
            if s.boss.enabled {
                enabled.push("boss");
            }
            if s.upgrade.enabled {
                enabled.push("upgrade");
            }

            // Saying this at startup is the whole point of the active-key gate: the
            // operator learns now that a feature they enabled cannot run.
            if keys.active.is_none() && (s.boss.enabled || s.upgrade.enabled) {
                warn!(
                    account = %account.name,
                    "boss fights and/or upgrades are enabled but no active key is in the wallet -- \
                     they will be skipped every cycle"
                );
            }

            info!(
                account = %account.name,
                active_key = keys.active.is_some(),
                actions = %enabled.join(", "),
                "ready",
            );
        }
        Ok(())
    }

    /// The blacklist as of now: configured names, plus every account in this config
    /// when `skip_own_accounts` is set. Remote lists are merged by `refresh_blacklist`.
    fn rebuild_blacklist(&mut self) {
        let mut set: HashSet<String> = self
            .config
            .blacklist
            .accounts
            .iter()
            .map(|n| n.trim().to_ascii_lowercase())
            .filter(|n| !n.is_empty())
            .collect();
        if self.config.blacklist.skip_own_accounts {
            for name in self.config.account_names() {
                set.insert(name.to_ascii_lowercase());
            }
        }
        self.blacklist = set;
    }

    /// Merge the remote lists in. A failure keeps whatever was already there: an
    /// unreachable list must never widen the target set.
    fn refresh_blacklist(&mut self) {
        if self.config.blacklist.urls.is_empty() {
            return;
        }
        self.rebuild_blacklist();
        for url in self.config.blacklist.urls.clone() {
            match ureq::get(&url)
                .timeout(Duration::from_secs(15))
                .call()
                .map_err(anyhow::Error::new)
                .and_then(|r| r.into_string().map_err(anyhow::Error::new))
            {
                Ok(body) => {
                    let before = self.blacklist.len();
                    for line in body.lines() {
                        let name = line.trim().to_ascii_lowercase();
                        if !name.is_empty() && !name.starts_with('#') {
                            self.blacklist.insert(name);
                        }
                    }
                    info!(
                        %url,
                        added = self.blacklist.len() - before,
                        total = self.blacklist.len(),
                        "blacklist updated"
                    );
                }
                Err(e) => warn!(%url, error = %e, "could not fetch the blacklist; keeping the current one"),
            }
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn wait(&self, secs: u64) {
        for _ in 0..secs {
            if self.stopping() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Run every account once.
    pub fn cycle(&mut self) -> Result<()> {
        self.refresh_blacklist();

        if self.config.general.max_transaction_queue > 0 {
            match self.api.transaction_queue() {
                Ok(queue) if queue > self.config.general.max_transaction_queue => {
                    warn!(
                        queue,
                        limit = self.config.general.max_transaction_queue,
                        "the game's transaction queue is backed up; skipping this cycle"
                    );
                    return Ok(());
                }
                Ok(queue) => info!(queue, "game transaction queue"),
                Err(e) => warn!(error = %e, "could not read the transaction queue; continuing"),
            }
        }

        // One block reference for the whole cycle. TaPoS stays valid far longer than
        // a cycle takes, and this keeps the node off the signing path entirely.
        self.hive
            .refresh_tapos()
            .context("could not reach any Hive node")?;

        let accounts = self.config.accounts.clone();
        for account in accounts {
            if self.stopping() {
                break;
            }
            if !account.enabled {
                continue;
            }
            if let Err(e) = self.run_account(&account) {
                warn!(account = %account.name, error = %format!("{e:#}"), "account failed; moving on");
            }
            self.wait(self.config.general.account_delay_secs);
        }
        Ok(())
    }

    fn run_account(&mut self, account: &crate::config::Account) -> Result<()> {
        let keys = self.keys.for_account(&account.name)?;
        let player = self
            .api
            .player(&account.name)
            .with_context(|| format!("reading the state of @{}", account.name))?;

        info!(
            account = %account.name,
            level = player.level,
            attacks = player.attacks,
            claims = player.claims,
            stash = format!("{:.2}/{:.2}", player.scrap, player.stash_capacity()),
            wallet = format!("{:.2} SCRAP", player.hive_engine_scrap),
            flux = format!("{:.4}", player.flux),
            damage = format!("{:.0}", player.stats.damage),
            "state",
        );

        let runner = Runner {
            api: &self.api,
            hive: &self.hive,
            account: &account.name,
            keys: &keys,
            settings: &account.settings,
            blacklist: &self.blacklist,
            stop: Arc::clone(&self.stop),
        };

        // A full stash makes every attack pointless, so empty it first. The rest of
        // the ordering is: fight, bank what was won, then spend.
        if account.settings.claim.claim_when_stash_full
            && player.stash_is_full()
            && self.claim_cooldown_ok(&account.name, &account.settings)
        {
            match runner.claim(&player) {
                Ok(outcome) => self.note("pre-claim", &account.name, &outcome),
                Err(e) => log_failure(&account.name, "pre-claim", &e),
            }
            self.last_claim.insert(account.name.clone(), Instant::now());
            // Give the chain a moment before attacking against the new stash.
            self.wait(account.settings.claim.cooldown_secs.min(30));
        }

        match runner.attack(&player) {
            Ok(outcome) => self.note("attack", &account.name, &outcome),
            Err(e) => log_failure(&account.name, "attack", &e),
        }

        // Re-read: attacking moves the stash, the attack count and the claim count.
        let player = self.api.player(&account.name).unwrap_or(player);

        if self.claim_cooldown_ok(&account.name, &account.settings) {
            match runner.claim(&player) {
                Ok(outcome) => {
                    if outcome.performed > 0 {
                        self.last_claim.insert(account.name.clone(), Instant::now());
                    }
                    self.note("claim", &account.name, &outcome);
                }
                Err(e) => log_failure(&account.name, "claim", &e),
            }
        }

        match runner.quests() {
            Ok(outcome) => self.note("quests", &account.name, &outcome),
            Err(e) => log_failure(&account.name, "quests", &e),
        }

        match runner.upgrades(&player) {
            Ok(outcome) => self.note("upgrade", &account.name, &outcome),
            Err(e) => log_failure(&account.name, "upgrade", &e),
        }

        match runner.boss_fights() {
            Ok(outcome) => self.note("boss", &account.name, &outcome),
            Err(e) => log_failure(&account.name, "boss", &e),
        }

        Ok(())
    }

    fn claim_cooldown_ok(&self, account: &str, settings: &crate::config::Settings) -> bool {
        match self.last_claim.get(account) {
            Some(at) => at.elapsed() >= Duration::from_secs(settings.claim.cooldown_secs),
            None => true,
        }
    }

    fn note(&self, action: &str, account: &str, outcome: &Outcome) {
        match &outcome.skipped_reason {
            Some(reason) => info!(account, action, "skipped: {reason}"),
            None => info!(account, action, count = outcome.performed, "done"),
        }
    }

    /// The daemon loop.
    pub fn run(&mut self) -> Result<()> {
        let interval = self.config.general.cycle_interval_secs;
        loop {
            let started = Instant::now();
            if let Err(e) = self.cycle() {
                warn!(error = %format!("{e:#}"), "cycle failed");
            }
            if self.stopping() {
                info!("stopping");
                return Ok(());
            }
            let elapsed = started.elapsed().as_secs();
            let sleep = interval.saturating_sub(elapsed).max(30);
            info!(seconds = sleep, "cycle complete; sleeping");
            self.wait(sleep);
            if self.stopping() {
                info!("stopping");
                return Ok(());
            }
        }
    }

    /// Read-only: what the bot sees and what it would do, without broadcasting.
    pub fn status(&mut self) -> Result<()> {
        self.refresh_blacklist();
        for account in &self.config.accounts {
            let keys = self.keys.for_account(&account.name);
            let player = match self.api.player(&account.name) {
                Ok(p) => p,
                Err(e) => {
                    println!("@{:<16} unreachable: {e:#}", account.name);
                    continue;
                }
            };
            println!(
                "@{name}\n  \
                 level {level:.0}   attacks {attacks:.0}   claims {claims:.0}\n  \
                 stash   {scrap:.4} / {cap:.4}{full}\n  \
                 wallet  {wallet:.4} SCRAP liquid, {stake:.4} staked, {flux:.4} FLUX\n  \
                 stats   damage {dmg:.0}  defense {def:.0}  engineering {eng:.0}  dodge {dodge:.1}%\n  \
                 keys    posting {posting}, active {active}\n  \
                 enabled {enabled}",
                name = account.name,
                level = player.level,
                attacks = player.attacks,
                claims = player.claims,
                scrap = player.scrap,
                cap = player.stash_capacity(),
                full = if player.stash_is_full() { "  (FULL)" } else { "" },
                wallet = player.hive_engine_scrap,
                stake = player.hive_engine_stake,
                flux = player.flux,
                dmg = player.stats.damage,
                def = player.stats.defense,
                eng = player.stats.engineering,
                dodge = player.stats.dodge,
                posting = match &keys {
                    Ok(_) => "yes",
                    Err(_) => "MISSING",
                },
                active = match &keys {
                    Ok(k) if k.active.is_some() => "yes",
                    _ => "no",
                },
                enabled = {
                    let s = &account.settings;
                    let mut v = Vec::new();
                    if s.attack.enabled {
                        v.push("attack");
                    }
                    if s.claim.enabled {
                        v.push("claim");
                    }
                    if s.quest.enabled && s.quest.collect {
                        v.push("quests");
                    }
                    if s.boss.enabled {
                        v.push("boss");
                    }
                    if s.upgrade.enabled {
                        v.push("upgrade");
                    }
                    if v.is_empty() {
                        "nothing".to_string()
                    } else {
                        v.join(", ")
                    }
                },
            );
        }
        println!("\n{} accounts on the blacklist", self.blacklist.len());
        Ok(())
    }
}
