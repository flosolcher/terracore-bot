//! One cycle over every configured account, and the loop around it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::actions::{log_failure, Outcome, Runner};
use crate::api::Api;
use crate::config::{Config, Stat};
use crate::hive::Broadcaster;
use crate::keys::KeyStore;
use crate::targeting::Context as TargetContext;

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
}

/// Read-only: what the bot sees for each account, and what it is allowed to do.
///
/// Deliberately does not unlock the wallet. Which accounts hold which roles is
/// metadata stored in the clear, so answering "do I have an active key for bob"
/// costs nothing and needs no passphrase.
pub fn status(config: &Config) -> Result<()> {
    let api = Api::new(
        &config.terracore.api,
        Duration::from_secs(config.terracore.timeout_secs),
        config.terracore.retries,
    );
    let held = crate::keys::list(&config.wallet_path()).unwrap_or_default();

    for account in &config.accounts {
        let roles = held.get(&account.name).cloned().unwrap_or_default();
        let has = |role: &str| roles.iter().any(|r| r == role);

        let player = match api.player(&account.name) {
            Ok(player) => player,
            Err(e) => {
                println!("@{}  unreachable: {:#}\n", account.name, e);
                continue;
            }
        };

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
        // An enabled feature with no key is not enabled, and saying so here is the
        // point of the command.
        if s.boss.enabled {
            enabled.push(if has("active") { "boss" } else { "boss (NO ACTIVE KEY)" });
        }
        if s.upgrade.enabled {
            enabled.push(if has("active") { "upgrade" } else { "upgrade (NO ACTIVE KEY)" });
        }

        println!(
            "@{name}{disabled}\n  \
             level {level:.0}   attacks {attacks:.0}   claims {claims:.0}\n  \
             stash    {scrap:.4} / {cap:.4}{full}\n  \
             wallet   {wallet:.4} SCRAP liquid, {stake:.4} staked, {flux:.4} FLUX\n  \
             stats    damage {dmg:.0}, defense {def:.0}, engineering {eng:.0}, dodge {dodge:.1}%\n  \
             next up  engineering {eng_cost:.0}, damage {dmg_cost:.0}, defense {def_cost:.0} SCRAP\n  \
             keys     posting {posting}, active {active}\n  \
             does     {enabled}\n",
            name = account.name,
            disabled = if account.enabled { "" } else { "   (disabled)" },
            level = player.level,
            attacks = player.attacks,
            claims = player.claims,
            scrap = player.scrap,
            cap = player.stash_capacity(),
            full = if player.stash_is_full() { "   STASH FULL" } else { "" },
            wallet = player.hive_engine_scrap,
            stake = player.hive_engine_stake,
            flux = player.flux,
            dmg = player.stats.damage,
            def = player.stats.defense,
            eng = player.stats.engineering,
            dodge = player.stats.dodge,
            eng_cost = crate::actions::upgrade_cost(Stat::Engineering, player.engineering),
            dmg_cost = crate::actions::upgrade_cost(Stat::Damage, player.damage),
            def_cost = crate::actions::upgrade_cost(Stat::Defense, player.defense),
            posting = if has("posting") { "yes" } else { "MISSING" },
            active = if has("active") { "yes" } else { "no" },
            enabled = if enabled.is_empty() { "nothing".to_string() } else { enabled.join(", ") },
        );
    }
    Ok(())
}

/// What the bot would attack right now, and why it refused everyone else.
///
/// "No opponent found" is not a diagnosis. This answers the question the old bot
/// could not: whether the board is empty because everyone out-defends you, because
/// they are all shielded, or because your own filters are too tight.
pub fn targets(config: &Config, only: Option<&str>, show: usize) -> Result<()> {
    let api = Api::new(
        &config.terracore.api,
        Duration::from_secs(config.terracore.timeout_secs),
        config.terracore.retries,
    );

    let mut blacklist: HashSet<String> = config
        .blacklist
        .accounts
        .iter()
        .map(|n| n.trim().to_ascii_lowercase())
        .filter(|n| !n.is_empty())
        .collect();
    if config.blacklist.skip_own_accounts {
        for name in config.account_names() {
            blacklist.insert(name.to_ascii_lowercase());
        }
    }

    for account in &config.accounts {
        if only.is_some_and(|want| !want.eq_ignore_ascii_case(&account.name)) {
            continue;
        }
        let player = match api.player(&account.name) {
            Ok(player) => player,
            Err(e) => {
                println!("@{}  unreachable: {:#}\n", account.name, e);
                continue;
            }
        };

        let settings = &account.settings.attack;
        let ctx = TargetContext {
            me: &account.name,
            my_damage: player.stats.damage,
            focus_charges: player.focus_charges(),
            settings,
            blacklist: &blacklist,
            now: crate::api::now_ms(),
        };
        let board = api.battles(
            player.stats.damage,
            settings.candidate_limit,
            1,
            ctx.focus_active(),
        )?;
        let ranked = crate::targeting::rank(&board, &ctx);

        println!(
            "@{}  damage {:.0}, {} attacks, {} claims -- {} rows on the board, {} reachable",
            account.name,
            player.stats.damage,
            player.attacks as u32,
            player.claims as u32,
            board.len(),
            ranked.len()
        );
        for (i, target) in ranked.iter().take(show).enumerate() {
            println!(
                "  {:>2}. @{:<17} {:>12.2} scrap  {:>12.2} expected  defense {:>7.0}  dodge {:>5.1}%",
                i + 1,
                target.username,
                target.scrap,
                target.expected_scrap(),
                target.defense(),
                target.dodge(),
            );
        }
        let tally = crate::targeting::tally(&board, &ctx);
        if !tally.is_empty() {
            println!(
                "  refused: {}",
                tally
                    .iter()
                    .map(|(r, n)| format!("{n} {}", r.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        println!();
    }
    Ok(())
}
