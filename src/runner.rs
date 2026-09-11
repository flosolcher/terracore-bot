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
use crate::state::{epoch_secs, AccountStatus, ActionResult, PlayerSnapshot, Shared};
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
    /// What the control panel reads and writes. Present whether or not the panel is
    /// running, so the bot has one code path rather than two.
    shared: Arc<Shared>,
}

impl Bot {
    pub fn new(config: Config, stop: Arc<AtomicBool>, shared: Arc<Shared>) -> Result<Self> {
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
            shared,
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
            let enabled = s.enabled_actions();

            // Saying this at startup is the whole point of the active-key gate: the
            // operator learns now that a feature they enabled cannot run.
            if keys.active.is_none() && s.needs_active_key() {
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
                Err(e) => {
                    warn!(%url, error = %e, "could not fetch the blacklist; keeping the current one")
                }
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
        self.reload_if_asked();
        self.refresh_blacklist();

        {
            let mut board = self.shared.status();
            board.cycle += 1;
            board.cycle_started = epoch_secs();
            board.running = true;
            board.dry_run = self.hive.is_dry_run();
            board.blacklist_size = self.blacklist.len();
        }
        // Whatever happens below -- an unreachable node, a panic upstream -- the
        // panel must not be left saying "running" forever.
        let _finish = FinishCycle(Arc::clone(&self.shared));

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
            let status = match self.run_account(&account) {
                Ok(status) => status,
                Err(e) => {
                    let message = format!("{e:#}");
                    warn!(account = %account.name, error = %message, "account failed; moving on");
                    AccountStatus {
                        at: epoch_secs(),
                        error: Some(message),
                        ..Default::default()
                    }
                }
            };
            self.shared
                .status()
                .accounts
                .insert(account.name.clone(), status);
            self.wait(self.config.general.account_delay_secs);
        }
        Ok(())
    }

    fn run_account(&mut self, account: &crate::config::Account) -> Result<AccountStatus> {
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

        let mut report = AccountStatus {
            at: epoch_secs(),
            player: Some(PlayerSnapshot::from(&player)),
            ..Default::default()
        };

        let runner = Runner {
            api: &self.api,
            hive: &self.hive,
            account: &account.name,
            keys: &keys,
            settings: &account.settings,
            blacklist: &self.blacklist,
            stop: Arc::clone(&self.stop),
        };

        // Consumables go first. The game queues custom_json, so a potion used now is
        // not visible for a while -- it buys attacks for the next cycle rather than
        // this one. Which is exactly why it should not wait: run it last and the
        // potion lands a whole cycle later still.
        match runner.use_consumables(&player) {
            Ok(outcome) => self.note("consumables", &account.name, &outcome, &mut report),
            Err(e) => self.note_failure("consumables", &account.name, &e, &mut report),
        }

        // A full stash makes every attack pointless, so empty it first. The rest of
        // the ordering is: fight, bank what was won, then spend.
        if account.settings.claim.claim_when_stash_full
            && player.stash_is_full()
            && self.claim_cooldown_ok(&account.name, &account.settings)
        {
            match runner.claim(&player) {
                Ok(outcome) => self.note("pre-claim", &account.name, &outcome, &mut report),
                Err(e) => self.note_failure("pre-claim", &account.name, &e, &mut report),
            }
            self.last_claim.insert(account.name.clone(), Instant::now());
            // Give the chain a moment before attacking against the new stash.
            self.wait(account.settings.claim.cooldown_secs.min(30));
        }

        match runner.attack(&player) {
            Ok(outcome) => self.note("attack", &account.name, &outcome, &mut report),
            Err(e) => self.note_failure("attack", &account.name, &e, &mut report),
        }

        // Re-read: attacking moves the stash, the attack count and the claim count.
        let player = self.api.player(&account.name).unwrap_or(player);
        report.player = Some(PlayerSnapshot::from(&player));

        if self.claim_cooldown_ok(&account.name, &account.settings) {
            match runner.claim(&player) {
                Ok(outcome) => {
                    if outcome.performed > 0 {
                        self.last_claim.insert(account.name.clone(), Instant::now());
                    }
                    self.note("claim", &account.name, &outcome, &mut report);
                }
                Err(e) => self.note_failure("claim", &account.name, &e, &mut report),
            }
        }

        match runner.quests() {
            Ok(outcome) => self.note("quests", &account.name, &outcome, &mut report),
            Err(e) => self.note_failure("quests", &account.name, &e, &mut report),
        }

        match runner.open_crates() {
            Ok(outcome) => self.note("crates", &account.name, &outcome, &mut report),
            Err(e) => self.note_failure("crates", &account.name, &e, &mut report),
        }

        // Both of the next two spend liquid SCRAP, and the game will still be
        // reporting the pre-mission balance when the second one asks. Carrying what
        // was already committed across is what stops them promising the same SCRAP
        // twice and having the second lot rejected on-chain.
        let mut committed = 0.0;
        match runner.start_missions(&player) {
            Ok(outcome) => {
                committed += outcome.spent;
                self.note("missions", &account.name, &outcome, &mut report);
            }
            Err(e) => self.note_failure("missions", &account.name, &e, &mut report),
        }

        if committed > 0.0 {
            info!(
                account = %account.name,
                committed = format!("{committed:.0}"),
                "already committed this cycle; spending sees the reduced balance"
            );
        }

        match runner.spend(&player, committed) {
            Ok(outcome) => self.note("spend", &account.name, &outcome, &mut report),
            Err(e) => self.note_failure("spend", &account.name, &e, &mut report),
        }

        match runner.boss_fights() {
            Ok(outcome) => self.note("boss", &account.name, &outcome, &mut report),
            Err(e) => self.note_failure("boss", &account.name, &e, &mut report),
        }

        Ok(report)
    }

    fn claim_cooldown_ok(&self, account: &str, settings: &crate::config::Settings) -> bool {
        match self.last_claim.get(account) {
            Some(at) => at.elapsed() >= Duration::from_secs(settings.claim.cooldown_secs),
            None => true,
        }
    }

    fn note(&self, action: &str, account: &str, outcome: &Outcome, report: &mut AccountStatus) {
        match &outcome.skipped_reason {
            Some(reason) => info!(account, action, "skipped: {reason}"),
            None => info!(account, action, count = outcome.performed, "done"),
        }
        report.actions.push(ActionResult {
            action: action.to_string(),
            count: outcome.performed,
            skipped: outcome.skipped_reason.clone(),
            failed: None,
        });
    }

    fn note_failure(
        &self,
        action: &str,
        account: &str,
        error: &anyhow::Error,
        report: &mut AccountStatus,
    ) {
        log_failure(account, action, error);
        report.actions.push(ActionResult {
            action: action.to_string(),
            count: 0,
            skipped: None,
            failed: Some(format!("{error:#}")),
        });
    }

    /// Pick up a config the panel wrote.
    ///
    /// Only the parts that can change under a running bot are applied. The node list,
    /// the wallet and the dry-run flag shape objects built once at start-up, so a
    /// change there is reported rather than half-applied.
    fn reload_if_asked(&mut self) {
        if !self.shared.control().take_reload() {
            return;
        }
        match Config::load(&self.config.path) {
            Ok(new) => {
                if new.hive.nodes != self.config.hive.nodes
                    || new.wallet.path != self.config.wallet.path
                    || new.terracore.api != self.config.terracore.api
                    || new.general.dry_run != self.config.general.dry_run
                {
                    warn!(
                        "[hive], [wallet], [terracore] and dry_run are read once at start-up -- \
                         restart to apply those. Account settings have been reloaded."
                    );
                }
                self.config.accounts = new.accounts;
                self.config.blacklist = new.blacklist;
                self.config.general.cycle_interval_secs = new.general.cycle_interval_secs;
                self.config.general.account_delay_secs = new.general.account_delay_secs;
                self.config.general.max_transaction_queue = new.general.max_transaction_queue;
                self.rebuild_blacklist();
                info!(accounts = self.config.accounts.len(), "config reloaded");
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), "the edited config would not load; keeping the running one")
            }
        }
    }

    /// The daemon loop.
    pub fn run(&mut self) -> Result<()> {
        let mut was_paused = false;
        loop {
            let paused = self.shared.control().paused;
            if paused {
                if !was_paused {
                    info!("paused; no new cycles will start until resumed");
                }
                was_paused = true;
                // Safe to use the same sleep here: `take_run_now` refuses to consume
                // the flag while paused, so a request made now survives to resume.
                self.sleep_until_woken(5);
            } else {
                if was_paused {
                    info!("resumed");
                }
                was_paused = false;

                let started = Instant::now();
                if let Err(e) = self.cycle() {
                    warn!(error = %format!("{e:#}"), "cycle failed");
                }
                if self.stopping() {
                    info!("stopping");
                    return Ok(());
                }
                let elapsed = started.elapsed().as_secs();
                let sleep = self
                    .config
                    .general
                    .cycle_interval_secs
                    .saturating_sub(elapsed)
                    .max(30);
                info!(seconds = sleep, "cycle complete; sleeping");
                self.sleep_until_woken(sleep);
            }
            if self.stopping() {
                info!("stopping");
                return Ok(());
            }
        }
    }

    /// Sleep, but wake early for a stop or for the panel's "run now".
    fn sleep_until_woken(&self, secs: u64) {
        for _ in 0..secs {
            if self.stopping() {
                return;
            }
            if self.shared.control().take_run_now() {
                info!("running a cycle on request");
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
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
        // An enabled feature with no key is not enabled, and saying so here is the
        // point of the command.
        let mut enabled: Vec<String> = s.enabled_actions().iter().map(|a| a.to_string()).collect();
        if s.needs_active_key() && !has("active") {
            for action in enabled.iter_mut() {
                if action == "boss" || action == "spend" {
                    action.push_str(" (NO ACTIVE KEY)");
                }
            }
        }

        println!(
            "@{name}{disabled}\n  \
             level {level:.0}   attacks {attacks:.0}   claims {claims:.0}\n  \
             stash    {scrap:.4} / {cap:.4}{full}\n  \
             wallet   {wallet:.4} SCRAP liquid, {stake:.4} staked, {flux:.4} FLUX\n  \
             stats    damage {dmg:.0}, defense {def:.0}, engineering {eng:.0}, dodge {dodge:.1}%\n  \
             next up  engineering {eng_cost:.0} ({eng_days:.0}d payback), damage {dmg_cost:.0}, defense {def_cost:.0}\n  \
             stake    {staked:.0} -> dodge {c_dodge:.2}%, luck {c_luck:.2}%; next dodge point {dodge_cost:.0}\n  \
             favor    {favor:.0} -> crit {c_crit:.3}%; next crit point {crit_cost:.0}\n  \
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
            eng_cost = crate::curves::stat_cost(Stat::Engineering, player.engineering),
            dmg_cost = crate::curves::stat_cost(Stat::Damage, player.damage),
            def_cost = crate::curves::stat_cost(Stat::Defense, player.defense),
            eng_days = crate::curves::engineering_payback_days(player.engineering),
            staked = player.hive_engine_stake,
            // From the game's own curves rather than the API's numbers, so a drift
            // between the two is visible rather than hidden.
            c_dodge = crate::curves::dodge_from_stake(player.hive_engine_stake),
            c_luck = crate::curves::luck_from_stake(player.hive_engine_stake),
            dodge_cost = crate::curves::scrap_per_dodge_point(player.hive_engine_stake),
            favor = player.favor,
            c_crit = crate::curves::crit_from_favor(player.favor),
            crit_cost = crate::curves::scrap_per_crit_point(player.favor),
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

/// Clears the panel's "running" flag when a cycle ends, however it ends.
struct FinishCycle(Arc<Shared>);

impl Drop for FinishCycle {
    fn drop(&mut self) {
        let mut board = self.0.status();
        board.running = false;
        board.cycle_finished = epoch_secs();
    }
}
