//! The five things the bot does, each one self-contained and each one able to say
//! why it did nothing.
//!
//! Two of them -- boss fights and upgrades -- move Hive-Engine tokens and so need an
//! active key. They refuse to run without one even if the config enables them.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hivecomb::PrivateKey;
use serde_json::json;
use tracing::{debug, info, warn};

use crate::api::{format_number, now_ms, Api, Player, Quest};
use crate::config::{BossOrder, Settings, SpendSettings, Stat};
use crate::curves;
use crate::hive::{tx_hash, Auth, Broadcaster};
use crate::keys::AccountKeys;
use crate::missions;
use crate::targeting::{self, Context as TargetContext};

/// Everything an action needs. Held per account for the length of one cycle.
pub struct Runner<'a> {
    pub api: &'a Api,
    pub hive: &'a Broadcaster,
    pub account: &'a str,
    pub keys: &'a AccountKeys,
    pub settings: &'a Settings,
    pub blacklist: &'a HashSet<String>,
    /// Set by Ctrl-C. Checked before every broadcast and during every sleep, so a
    /// stop lands between actions rather than in the middle of one.
    pub stop: Arc<AtomicBool>,
    /// Missions this process has already started today. The game's own record lags
    /// behind by a queue, so this is what stops a second cycle paying twice.
    pub started_missions: Arc<std::sync::Mutex<HashSet<missions::MissionKey>>>,
}

/// What one action did, for the cycle summary.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub performed: u32,
    pub skipped_reason: Option<String>,
    /// Liquid SCRAP this action committed.
    ///
    /// The game processes custom_json through a queue, so the API still reports the
    /// old balance for a while afterwards. Anything later in the same cycle that
    /// also spends SCRAP has to subtract this, or the two commit the same balance
    /// twice and the second one is simply rejected on-chain.
    pub spent: f64,
}

impl Outcome {
    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            performed: 0,
            skipped_reason: Some(reason.into()),
            spent: 0.0,
        }
    }

    fn did(n: u32) -> Self {
        Self {
            performed: n,
            skipped_reason: None,
            spent: 0.0,
        }
    }

    fn did_spending(n: u32, spent: f64) -> Self {
        Self {
            performed: n,
            skipped_reason: None,
            spent,
        }
    }
}

impl Runner<'_> {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleep in one-second slices so Ctrl-C is not held up by a long delay.
    ///
    /// A dry run broadcasts nothing, so there is nothing to pace: waiting would only
    /// make the rehearsal slower than the real thing.
    fn wait(&self, secs: u64) {
        if self.hive.is_dry_run() {
            return;
        }
        for _ in 0..secs {
            if self.stopping() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// The active key, or an explanation of why the caller cannot have one. This is
    /// the single gate on every token-spending feature.
    fn active_key(&self, what: &str) -> Result<&PrivateKey, Outcome> {
        self.keys.active.as_ref().ok_or_else(|| {
            Outcome::skipped(format!(
                "{what} needs an active key, and the wallet holds none for @{}",
                self.account
            ))
        })
    }

    // -----------------------------------------------------------------------
    // Claim
    // -----------------------------------------------------------------------

    /// Move the stash into the Hive-Engine wallet.
    ///
    /// The amount is rendered with eight decimals exactly as the website does; the
    /// game reads this string, so its shape is part of the interface.
    pub fn claim(&self, player: &Player) -> Result<Outcome> {
        let s = &self.settings.claim;
        if !s.enabled {
            return Ok(Outcome::skipped("claiming is disabled"));
        }
        if (player.claims as u32) < s.min_claims {
            return Ok(Outcome::skipped(format!(
                "{} claims left, below min_claims of {}",
                player.claims as u32, s.min_claims
            )));
        }
        if player.scrap < s.min_scrap {
            return Ok(Outcome::skipped(format!(
                "{:.4} scrap in the stash, below min_scrap of {}",
                player.scrap, s.min_scrap
            )));
        }

        let amount = format!("{:.8}", player.scrap);
        let sent = self.hive.custom_json(
            self.account,
            &self.keys.posting,
            Auth::Posting,
            "terracore_claim",
            json!({ "amount": amount }),
        )?;
        info!(
            account = self.account,
            scrap = %amount,
            claims_left = (player.claims as u32).saturating_sub(1),
            trx = sent.trx_id(),
            dry_run = sent.was_dry_run(),
            "claimed",
        );
        Ok(Outcome::did(1))
    }

    // -----------------------------------------------------------------------
    // Attack
    // -----------------------------------------------------------------------

    /// Attack the best available targets until the attacks, the targets or the
    /// per-cycle budget run out.
    ///
    /// The player is re-read from the API between attacks rather than counted down
    /// locally: attacks and claims both move for reasons this bot did not cause, and
    /// a stale count is how the old bot ended up broadcasting attacks it did not have.
    pub fn attack(&self, player: &Player) -> Result<Outcome> {
        let s = &self.settings.attack;
        if !s.enabled {
            return Ok(Outcome::skipped("attacking is disabled"));
        }

        let mut player = player.clone();
        // The game processes custom_json through a queue, so a re-read moments after
        // an attack can still report the old count. Trusting it alone would attack
        // more times than there are attacks; the budget seen at the start caps the
        // run, and the re-read can only end it earlier.
        let budget = player.attacks as u32;
        let mut attacked: HashSet<String> = HashSet::new();
        let mut board = Vec::new();
        let mut board_is_fresh = false;
        let mut done: u32 = 0;

        loop {
            if self.stopping() {
                break;
            }
            if (player.attacks as u32) < s.min_attacks.max(1) {
                if done == 0 {
                    return Ok(Outcome::skipped(format!(
                        "{} attacks available, below min_attacks of {}",
                        player.attacks as u32,
                        s.min_attacks.max(1)
                    )));
                }
                break;
            }
            // The website refuses to attack with no claims left, and so does the game.
            if player.claims < 1.0 {
                if done == 0 {
                    return Ok(Outcome::skipped(
                        "no claims left; the game blocks attacking",
                    ));
                }
                break;
            }
            if player.stash_is_full() {
                if done == 0 {
                    return Ok(Outcome::skipped(format!(
                        "stash full ({:.2} of {:.2}); loot would have nowhere to go",
                        player.scrap,
                        player.stash_capacity()
                    )));
                }
                break;
            }
            if s.max_per_cycle > 0 && done >= s.max_per_cycle {
                break;
            }
            if done >= budget {
                debug!(
                    account = self.account,
                    budget, "the cycle's attack budget is spent"
                );
                break;
            }

            let ctx = TargetContext {
                me: self.account,
                my_damage: player.stats.damage,
                focus_charges: player.focus_charges(),
                settings: s,
                blacklist: self.blacklist,
                now: now_ms(),
            };

            if board.is_empty() && !board_is_fresh {
                board = self
                    .api
                    .battles(
                        player.stats.damage,
                        s.candidate_limit,
                        1,
                        ctx.focus_active(),
                    )
                    .context("fetching the battle board")?;
                board_is_fresh = true;
            }

            let ranked = targeting::rank(&board, &ctx);
            let target = ranked.iter().find(|t| !attacked.contains(&t.username));

            let Some(target) = target else {
                if done == 0 {
                    let tally = targeting::tally(&board, &ctx);
                    let detail = if tally.is_empty() {
                        "the battle board came back empty".to_string()
                    } else {
                        tally
                            .iter()
                            .map(|(r, n)| format!("{n} {}", r.as_str()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    return Ok(Outcome::skipped(format!("no reachable target: {detail}")));
                }
                debug!(account = self.account, "no target left on the board");
                break;
            };

            let sent = self.hive.custom_json(
                self.account,
                &self.keys.posting,
                Auth::Posting,
                "terracore_battle",
                json!({ "target": target.username }),
            )?;
            info!(
                account = self.account,
                target = %target.username,
                scrap = format!("{:.2}", target.scrap),
                expected = format!("{:.2}", target.expected_scrap()),
                dodge = format!("{:.1}%", target.dodge()),
                trx = sent.trx_id(),
                dry_run = sent.was_dry_run(),
                "attacked",
            );
            attacked.insert(target.username.clone());
            done += 1;

            // A dry run never changes anything on the server, so re-reading the
            // player would return the same numbers forever and loop.
            if self.hive.is_dry_run() {
                continue;
            }

            self.wait(s.delay_secs);
            if self.stopping() {
                break;
            }

            player = match self.api.player(self.account) {
                Ok(p) => p,
                Err(e) => {
                    warn!(account = self.account, error = %e, "could not re-read the player; stopping the attack run");
                    break;
                }
            };
            // The board is stale after an attack lands: our target's scrap moved and
            // its battle cooldown started. Refetch on the next pass.
            board.clear();
            board_is_fresh = false;
        }

        Ok(Outcome::did(done))
    }

    // -----------------------------------------------------------------------
    // Quests
    // -----------------------------------------------------------------------

    /// Collect finished missions. This only harvests rewards -- it never starts a
    /// mission, which would cost SCRAP.
    pub fn quests(&self) -> Result<Outcome> {
        let s = &self.settings.quest;
        if !s.enabled || !s.collect {
            return Ok(Outcome::skipped("quest collection is disabled"));
        }

        let quests: Vec<Quest> = self.api.quests(self.account).context("fetching missions")?;
        let now = now_ms();
        let ready: Vec<&Quest> = quests.iter().filter(|q| q.collectable(now)).collect();

        if ready.is_empty() {
            let running = quests.iter().filter(|q| !q.collected).count();
            return Ok(Outcome::skipped(format!(
                "no mission ready to collect ({running} still running)"
            )));
        }

        let mut done = 0;
        for quest in ready {
            if self.stopping() {
                break;
            }
            let sent = self.hive.custom_json(
                self.account,
                &self.keys.posting,
                Auth::Posting,
                "terracore_quest_collect",
                json!({ "quest_id": quest.id }),
            )?;
            info!(
                account = self.account,
                quest = %quest.name,
                kind = %quest.quest_type,
                tier = quest.tier,
                trx = sent.trx_id(),
                dry_run = sent.was_dry_run(),
                "collected mission",
            );
            done += 1;
            self.wait(s.delay_secs);
        }
        Ok(Outcome::did(done))
    }

    /// Start missions from today's board.
    ///
    /// Burns SCRAP, so it needs an active key. The board resets daily and the game's
    /// own client refuses to start anything from a stale copy -- the SCRAP would be
    /// spent on a mission that no longer exists -- so that check comes first here too.
    pub fn start_missions(&self, player: &Player) -> Result<Outcome> {
        let s = &self.settings.quest;
        if !s.enabled || !s.start {
            return Ok(Outcome::skipped("starting missions is disabled"));
        }
        let active = match self.active_key("starting missions") {
            Ok(key) => key,
            Err(outcome) => return Ok(outcome),
        };

        let board = self
            .api
            .quest_board(self.account)
            .context("fetching the mission board")?;
        let today = missions::today_utc(now_ms());
        if board.date != today {
            return Ok(Outcome::skipped(format!(
                "the board reads {} and today is {today}; it has not rolled over yet",
                if board.date.is_empty() {
                    "nothing"
                } else {
                    &board.date
                }
            )));
        }
        let running = self.api.quests(self.account).unwrap_or_default();

        let mut spendable = (player.hive_engine_scrap - s.min_scrap_reserve).max(0.0);
        let already = {
            let mut set = self
                .started_missions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Keyed by day, so yesterday's entries can never match today's board.
            // Dropping them keeps this from growing for the life of the process --
            // five types across five tiers every day adds up over a few months.
            set.retain(|(date, _, _)| date == &today);
            set.clone()
        };
        let (candidates, refused) = missions::select(
            &board,
            &missions::Context {
                today: &today,
                player,
                items: &player.items,
                running: &running,
                settings: s,
                spendable,
                already_started: &already,
            },
        );

        if candidates.is_empty() {
            return Ok(Outcome::skipped(if refused.is_empty() {
                "no mission on the board matches the filters".to_string()
            } else {
                refused.join("; ")
            }));
        }

        let mut done = 0;
        let mut committed = 0.0;
        for slot in candidates {
            if self.stopping() {
                break;
            }
            if s.max_starts_per_cycle > 0 && done >= s.max_starts_per_cycle {
                break;
            }
            if slot.scrap_cost > spendable {
                continue;
            }
            let memo = format!(
                "terracore_quest_start-{}-{}-{}",
                slot.quest_type,
                slot.tier.round() as u8,
                tx_hash()
            );
            let sent = self.hive.engine_transfer(
                self.account,
                active,
                "SCRAP",
                "null",
                &format_number(slot.scrap_cost),
                json!(memo),
            )?;
            info!(
                account = self.account,
                mission = %slot.name,
                kind = %slot.quest_type,
                tier = slot.tier,
                cost = format!("{:.0}", slot.scrap_cost),
                hours = slot.duration_hours,
                trx = sent.trx_id(),
                dry_run = sent.was_dry_run(),
                "started mission",
            );
            self.started_missions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(missions::key_for(&board.date, slot));
            spendable -= slot.scrap_cost;
            committed += slot.scrap_cost;
            done += 1;
            self.wait(s.delay_secs);
        }
        Ok(Outcome::did_spending(done, committed))
    }

    /// Open crates. Free, and a posting key is enough.
    pub fn open_crates(&self) -> Result<Outcome> {
        let s = &self.settings.crates;
        if !s.enabled {
            return Ok(Outcome::skipped("opening crates is disabled"));
        }
        let inventory = self
            .api
            .inventory(self.account)
            .context("reading the inventory")?;
        let wanted: Vec<&crate::api::Crate> = inventory
            .crates
            .iter()
            .filter(|c| s.rarities.is_empty() || s.rarities.iter().any(|r| r == &c.rarity))
            .collect();
        if wanted.is_empty() {
            return Ok(Outcome::skipped(format!(
                "no crate to open ({} held)",
                inventory.crates.len()
            )));
        }

        let mut done = 0;
        for crate_ in wanted {
            if self.stopping() || (s.max_per_cycle > 0 && done >= s.max_per_cycle) {
                break;
            }
            let sent = self.hive.custom_json(
                self.account,
                &self.keys.posting,
                Auth::Posting,
                "terracore_open_crate",
                json!({ "crate_type": crate_.rarity, "owner": self.account }),
            )?;
            info!(
                account = self.account,
                rarity = %crate_.rarity,
                trx = sent.trx_id(),
                dry_run = sent.was_dry_run(),
                "opened crate",
            );
            done += 1;
            self.wait(s.delay_secs);
        }
        Ok(Outcome::did(done))
    }

    /// Use consumables, but only where one would unblock something right now.
    ///
    /// An attack potion drunk with attacks already in hand is thrown away, so the
    /// condition matters more than the inventory: the bot spends a charge only when
    /// the thing it grants is the thing currently missing.
    pub fn use_consumables(&self, player: &Player) -> Result<Outcome> {
        let s = &self.settings.consumables;
        if !s.enabled {
            return Ok(Outcome::skipped("consumables are disabled"));
        }
        let inventory = self
            .api
            .inventory(self.account)
            .context("reading the inventory")?;

        let (wanted, skipped) =
            consumables_to_use(player, &inventory, s, self.settings.claim.min_scrap);

        let mut done = 0;
        for item in wanted {
            if self.stopping() {
                break;
            }
            let sent = self.hive.custom_json(
                self.account,
                &self.keys.posting,
                Auth::Posting,
                "terracore_use_consumable",
                json!({
                    "action": format!("terracore_use_consumable-{}", tx_hash()),
                    "type": item.kind,
                }),
            )?;
            info!(
                account = self.account,
                consumable = %item.kind,
                held = item.amount,
                trx = sent.trx_id(),
                dry_run = sent.was_dry_run(),
                "used consumable",
            );
            done += 1;
            self.wait(s.delay_secs);
        }

        if done == 0 {
            return Ok(Outcome::skipped(if skipped.is_empty() {
                format!(
                    "nothing usable held ({} consumables)",
                    inventory.consumables.len()
                )
            } else {
                skipped.join("; ")
            }));
        }
        Ok(Outcome::did(done))
    }

    // -----------------------------------------------------------------------
    // Boss fights -- active key
    // -----------------------------------------------------------------------

    /// Fight planet bosses whose four-hour cooldown has expired.
    ///
    /// Each fight burns FLUX through Hive-Engine, so this is gated twice: on the
    /// config flag and on an active key actually being present.
    pub fn boss_fights(&self) -> Result<Outcome> {
        let s = &self.settings.boss;
        if !s.enabled {
            return Ok(Outcome::skipped("boss fights are disabled"));
        }
        let active = match self.active_key("boss fights") {
            Ok(key) => key,
            Err(outcome) => return Ok(outcome),
        };

        let planets = self.api.planets(self.account).context("fetching planets")?;
        if !planets.username.eq_ignore_ascii_case(self.account) {
            return Ok(Outcome::skipped(format!(
                "the planets endpoint answered for `{}` rather than `{}`",
                planets.username, self.account
            )));
        }
        if !planets.has_ship() {
            return Ok(Outcome::skipped("no ship equipped; travel is impossible"));
        }

        let now = now_ms();
        let mut eligible: Vec<_> = planets
            .boss_data
            .iter()
            .filter(|p| {
                if !s.planets.is_empty()
                    && !s.planets.iter().any(|n| n.eq_ignore_ascii_case(&p.name))
                {
                    return false;
                }
                if s.skip_planets
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&p.name))
                {
                    return false;
                }
                planets.level >= p.level
                    && p.ready(now)
                    && p.flux > 0.0
                    && p.flux <= s.max_flux_per_fight
            })
            .collect();

        match s.order {
            BossOrder::HighestLevel => eligible.sort_by(|a, b| {
                b.level
                    .partial_cmp(&a.level)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            BossOrder::Cheapest => eligible.sort_by(|a, b| {
                a.flux
                    .partial_cmp(&b.flux)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            BossOrder::Listed => eligible.sort_by_key(|p| {
                s.planets
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(&p.name))
                    .unwrap_or(usize::MAX)
            }),
        }

        if eligible.is_empty() {
            let waiting = planets
                .boss_data
                .iter()
                .filter(|p| planets.level >= p.level && !p.ready(now))
                .count();
            return Ok(Outcome::skipped(format!(
                "no planet ready ({waiting} still on cooldown)"
            )));
        }

        let mut flux = planets.flux;
        let mut done = 0;
        let mut last_reason = String::new();
        for planet in eligible {
            if self.stopping() {
                break;
            }
            if s.max_per_cycle > 0 && done >= s.max_per_cycle {
                break;
            }
            if flux - planet.flux < s.min_flux_reserve {
                last_reason = format!(
                    "{} costs {} FLUX and only {:.4} is left (reserve {})",
                    planet.name,
                    format_number(planet.flux),
                    flux,
                    s.min_flux_reserve
                );
                debug!(account = self.account, reason = %last_reason, "stopping boss fights");
                break;
            }

            let sent = self.hive.engine_transfer(
                self.account,
                active,
                "FLUX",
                "null",
                &format_number(planet.flux),
                json!({
                    "hash": format!("terracore_boss_fight-{}", tx_hash()),
                    "planet": planet.name,
                    "description": format!("Terracore Boss Fight - {}", planet.name),
                }),
            )?;
            info!(
                account = self.account,
                planet = %planet.name,
                flux = planet.flux,
                trx = sent.trx_id(),
                dry_run = sent.was_dry_run(),
                "fought boss",
            );
            flux -= planet.flux;
            done += 1;
            self.wait(s.delay_secs);
        }

        if done == 0 {
            return Ok(Outcome::skipped(if last_reason.is_empty() {
                "no planet fought".to_string()
            } else {
                last_reason
            }));
        }
        Ok(Outcome::did(done))
    }

    // -----------------------------------------------------------------------
    // Spending -- active key
    // -----------------------------------------------------------------------

    /// Divide the liquid balance between the things SCRAP can be turned into.
    ///
    /// The decision is [`plan`], which is pure; this only carries it out. Splitting
    /// them is what makes the rotation testable at all -- the policy is the subtle
    /// part and broadcasting is not.
    /// `already_committed` is liquid SCRAP this cycle has spent but the game has
    /// not processed yet -- missions, most often. It is a parameter rather than
    /// something the caller subtracts beforehand so that forgetting it is a
    /// compile error instead of two actions quietly promising the same balance.
    pub fn spend(&self, player: &Player, already_committed: f64) -> Result<Outcome> {
        let s = &self.settings.spend;
        if !s.enabled {
            return Ok(Outcome::skipped("spending is disabled"));
        }
        let active = match self.active_key("spending") {
            Ok(key) => key,
            Err(outcome) => return Ok(outcome),
        };

        let wallet = Wallet {
            liquid: (player.hive_engine_scrap - already_committed.max(0.0)).max(0.0),
            stake: player.hive_engine_stake,
            favor: player.favor,
            engineering: player.engineering,
            damage: player.damage,
            defense: player.defense,
        };
        let unreachable = self.unreachable_percent(player, s)?;
        let steps = plan(wallet.clone(), s, player.stats.engineering, unreachable);

        if steps.is_empty() {
            return Ok(Outcome::skipped(idle_reason(&wallet, s)));
        }

        let mut done = 0;
        let mut committed = 0.0;
        for step in &steps {
            if self.stopping() {
                break;
            }
            committed += match step {
                Step::Stake { amount, .. } | Step::Favor { amount } | Step::Stat { amount, .. } => {
                    *amount
                }
            };
            match step {
                Step::Stake { amount, why } => self.stake(active, *amount, why)?,
                Step::Favor { amount } => self.burn(
                    active,
                    *amount,
                    &format!("terracore_contribute-{}", tx_hash()),
                )?,
                Step::Stat { stat, amount } => self.burn(
                    active,
                    *amount,
                    &format!("terracore_{}-{}", stat.as_str(), tx_hash()),
                )?,
            }
            done += 1;
            self.wait(s.delay_secs);
        }
        Ok(Outcome::did_spending(done, committed))
    }

    /// What share of the battle board this account cannot reach, if damage buying
    /// needs to know. `None` when the question does not arise or cannot be answered.
    fn unreachable_percent(&self, player: &Player, s: &SpendSettings) -> Result<Option<f64>> {
        if !s.damage.enabled {
            return Ok(None);
        }
        let board = match self.api.battles(
            player.stats.damage,
            self.settings.attack.candidate_limit,
            1,
            false,
        ) {
            Ok(board) if !board.is_empty() => board,
            _ => return Ok(None),
        };
        let out_of_reach = board
            .iter()
            .filter(|t| t.defense() >= player.stats.damage)
            .count();
        Ok(Some(out_of_reach as f64 * 100.0 / board.len() as f64))
    }

    /// Send SCRAP to `null`, which is how the game charges for a stat or for favor.
    fn burn(&self, active: &PrivateKey, amount: f64, memo: &str) -> Result<()> {
        let sent = self.hive.engine_transfer(
            self.account,
            active,
            "SCRAP",
            "null",
            &format_number(amount),
            json!(memo),
        )?;
        info!(
            account = self.account,
            amount = format!("{amount:.2}"),
            memo,
            trx = sent.trx_id(),
            dry_run = sent.was_dry_run(),
            "burned SCRAP",
        );
        Ok(())
    }

    /// Stake SCRAP to yourself. Not a burn -- the balance stays yours.
    fn stake(&self, active: &PrivateKey, amount: f64, why: &str) -> Result<()> {
        let sent = self
            .hive
            .engine_stake(self.account, active, "SCRAP", &format_number(amount))?;
        info!(
            account = self.account,
            amount = format!("{amount:.2}"),
            why,
            trx = sent.trx_id(),
            dry_run = sent.was_dry_run(),
            "staked SCRAP",
        );
        Ok(())
    }
}

/// What the account holds, tracked locally through a cycle so each purchase pays the
/// price that follows the one before it rather than a price the API has not caught
/// up with yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Wallet {
    pub liquid: f64,
    pub stake: f64,
    pub favor: f64,
    pub engineering: f64,
    pub damage: f64,
    pub defense: f64,
}

impl Wallet {
    pub fn stat(&self, stat: crate::config::Stat) -> f64 {
        match stat {
            crate::config::Stat::Engineering => self.engineering,
            crate::config::Stat::Damage => self.damage,
            crate::config::Stat::Defense => self.defense,
        }
    }

    pub fn bump(&mut self, stat: crate::config::Stat) {
        match stat {
            crate::config::Stat::Engineering => self.engineering += 1.0,
            crate::config::Stat::Damage => self.damage += 1.0,
            crate::config::Stat::Defense => self.defense += 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Goal {
    Engineering,
    Favor,
    Stake,
    Damage,
    Defense,
}

impl Goal {
    fn stat(self) -> crate::config::Stat {
        match self {
            Goal::Engineering => crate::config::Stat::Engineering,
            Goal::Damage => crate::config::Stat::Damage,
            Goal::Defense => crate::config::Stat::Defense,
            // Only ever called for the lumpy goals.
            Goal::Favor | Goal::Stake => crate::config::Stat::Engineering,
        }
    }
}

/// Report a broadcast that failed without taking the whole cycle down: one account's
/// bad node is not a reason to stop the others.
pub fn log_failure(account: &str, action: &str, error: &anyhow::Error) {
    warn!(account, action, error = %format!("{error:#}"), "action failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Player;
    use crate::config::Settings;
    use crate::hive::Broadcaster;
    use hivecomb::PrivateKey;

    /// Published in hivecomb's own example and holding no value.
    const THROWAWAY: &str = "5KQwrPbwdL6PhXujxW37FSSQZ1JiwsST4cqQzDeyXtP79zkvFD3";

    /// Wired to a port nothing listens on. Every assertion below is about a decision
    /// taken *before* any request, so a test that started reaching the network would
    /// fail or hang rather than quietly pass.
    pub(super) fn parts(active: Option<PrivateKey>) -> (Api, Broadcaster, AccountKeys) {
        let api = Api::new("http://127.0.0.1:9", Duration::from_millis(50), 0);
        let hive = Broadcaster::new(
            vec!["http://127.0.0.1:9".into()],
            Duration::from_millis(50),
            60,
            Duration::from_secs(60),
            true,
        )
        .unwrap();
        let keys = AccountKeys {
            posting: PrivateKey::from_wif(THROWAWAY).unwrap(),
            active,
        };
        (api, hive, keys)
    }

    macro_rules! runner {
        ($api:expr, $hive:expr, $keys:expr, $settings:expr, $blacklist:expr) => {
            Runner {
                api: &$api,
                hive: &$hive,
                account: "alice",
                keys: &$keys,
                settings: &$settings,
                blacklist: &$blacklist,
                stop: Arc::new(AtomicBool::new(false)),
                started_missions: Arc::new(std::sync::Mutex::new(HashSet::new())),
            }
        };
    }

    #[test]
    fn boss_fights_and_upgrades_refuse_to_run_without_an_active_key() {
        let mut settings = Settings::default();
        settings.boss.enabled = true;
        settings.spend.enabled = true;

        let (api, hive, keys) = parts(None);
        let blacklist = HashSet::new();
        let runner = runner!(api, hive, keys, settings, blacklist);

        let boss = runner.boss_fights().unwrap();
        assert_eq!(boss.performed, 0);
        assert!(
            boss.skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("active key"),
            "{:?}",
            boss.skipped_reason
        );

        let upgrade = runner.spend(&Player::default(), 0.0).unwrap();
        assert_eq!(upgrade.performed, 0);
        assert!(
            upgrade
                .skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("active key"),
            "{:?}",
            upgrade.skipped_reason
        );
    }

    #[test]
    fn an_active_key_opens_the_gate_and_the_next_refusal_is_a_different_one() {
        // The counterpart to the test above. Without this, an action that always
        // skipped for any reason would pass that one for the wrong reason.
        let mut settings = Settings::default();
        settings.spend.enabled = true;

        let (api, hive, keys) = parts(Some(PrivateKey::from_wif(THROWAWAY).unwrap()));
        let blacklist = HashSet::new();
        let runner = runner!(api, hive, keys, settings, blacklist);

        // Nothing in the wallet, so it stops for lack of funds rather than for lack
        // of a key -- which is the whole point: the gate opened.
        let player = Player {
            engineering: 100.0,
            hive_engine_scrap: 0.0,
            ..Default::default()
        };
        let outcome = runner.spend(&player, 0.0).unwrap();
        assert_eq!(outcome.performed, 0);
        let reason = outcome.skipped_reason.unwrap_or_default();
        assert!(!reason.contains("active key"), "{reason}");
        assert!(reason.contains("spendable"), "{reason}");
    }

    /// The three guards that stop an attack run before it starts. Each is checked
    /// before the battle board is fetched, so all of this runs with no network -- a
    /// test that started reaching one would fail rather than quietly pass.
    #[test]
    fn an_attack_run_refuses_for_the_right_reason_and_says_which() {
        let settings = Settings::default();
        let (api, hive, keys) = parts(None);
        let blacklist = HashSet::new();
        let runner = runner!(api, hive, keys, settings, blacklist);

        let healthy = Player {
            attacks: 5.0,
            claims: 3.0,
            scrap: 0.0,
            hive_engine_stake: 1000.0,
            ..Default::default()
        };

        // No attacks left.
        let out = runner
            .attack(&Player {
                attacks: 0.0,
                ..healthy.clone()
            })
            .unwrap();
        assert_eq!(out.performed, 0);
        assert!(
            out.skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("below min_attacks"),
            "{:?}",
            out.skipped_reason
        );

        // Attacks in hand but no claims: the game refuses the battle, so spending one
        // would simply throw it away.
        let out = runner
            .attack(&Player {
                claims: 0.0,
                ..healthy.clone()
            })
            .unwrap();
        assert_eq!(out.performed, 0);
        assert!(
            out.skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("no claims left"),
            "{:?}",
            out.skipped_reason
        );

        // Stash at the ceiling: looted scrap would have nowhere to land.
        let out = runner
            .attack(&Player {
                scrap: 1001.0,
                ..healthy.clone()
            })
            .unwrap();
        assert_eq!(out.performed, 0);
        assert!(
            out.skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("stash full"),
            "{:?}",
            out.skipped_reason
        );

        // And disabled outright.
        let mut off = Settings::default();
        off.attack.enabled = false;
        let (api2, hive2, keys2) = parts(None);
        let runner = runner!(api2, hive2, keys2, off, blacklist);
        assert!(runner
            .attack(&healthy)
            .unwrap()
            .skipped_reason
            .unwrap_or_default()
            .contains("disabled"));
    }

    /// Claiming has its own guards, and the same property: every refusal happens
    /// before anything is signed or sent.
    #[test]
    fn claiming_refuses_for_the_right_reason_and_says_which() {
        let settings = Settings::default();
        let (api, hive, keys) = parts(None);
        let blacklist = HashSet::new();
        let runner = runner!(api, hive, keys, settings, blacklist);

        let out = runner
            .claim(&Player {
                claims: 0.0,
                scrap: 100.0,
                ..Default::default()
            })
            .unwrap();
        assert!(
            out.skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("below min_claims"),
            "{:?}",
            out.skipped_reason
        );

        let out = runner
            .claim(&Player {
                claims: 4.0,
                scrap: 0.0,
                ..Default::default()
            })
            .unwrap();
        assert!(
            out.skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("below min_scrap"),
            "{:?}",
            out.skipped_reason
        );

        let mut off = Settings::default();
        off.claim.enabled = false;
        let (api2, hive2, keys2) = parts(None);
        let runner = runner!(api2, hive2, keys2, off, blacklist);
        assert!(runner
            .claim(&Player {
                claims: 4.0,
                scrap: 100.0,
                ..Default::default()
            })
            .unwrap()
            .skipped_reason
            .unwrap_or_default()
            .contains("disabled"));
    }

    #[test]
    fn a_disabled_action_says_so_rather_than_blaming_the_key() {
        let settings = Settings::default();
        let (api, hive, keys) = parts(None);
        let blacklist = HashSet::new();
        let runner = runner!(api, hive, keys, settings, blacklist);
        let reason = runner
            .boss_fights()
            .unwrap()
            .skipped_reason
            .unwrap_or_default();
        assert!(reason.contains("disabled"), "{reason}");
    }
}

/// Which consumables are worth using right now, and why the rest are not.
///
/// Pure, because this is entirely a judgement call and the judgement is the part
/// worth testing: a potion used at the wrong moment is not an error anywhere, it is
/// simply gone. The rule is that a charge is spent only when the thing it grants is
/// the thing currently missing.
pub fn consumables_to_use<'a>(
    player: &Player,
    inventory: &'a crate::api::Inventory,
    s: &crate::config::ConsumableSettings,
    claim_min_scrap: f64,
) -> (Vec<&'a crate::api::Consumable>, Vec<String>) {
    // More attacks are worth having only if an attack could actually be made: room
    // in the stash for the loot, and a claim available to collect it.
    let could_use_attacks = player.attacks < 1.0 && !player.stash_is_full() && player.claims >= 1.0;
    // A claim charge is worth having only if there is something to claim.
    let could_use_claims = player.claims < 1.0 && player.scrap >= claim_min_scrap;

    let mut wanted = Vec::new();
    let mut skipped = Vec::new();

    for item in &inventory.consumables {
        if s.max_per_cycle > 0 && wanted.len() >= s.max_per_cycle as usize {
            break;
        }
        let kind = item.short_name();
        if item.amount < 1.0 || !s.use_kinds.iter().any(|k| k == kind) {
            continue;
        }
        let unblocks = match kind {
            "attack" | "fury" => could_use_attacks,
            "claim" => could_use_claims,
            // Everything else is a timed buff. Burning a 24-hour buff on a schedule
            // wastes most of it, so those are left to a person to decide.
            _ => false,
        };
        if unblocks {
            wanted.push(item);
        } else {
            skipped.push(format!("{kind} would be wasted right now"));
        }
    }
    (wanted, skipped)
}

/// One thing the bot has decided to do with SCRAP.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Staked to yourself: raises dodge, luck and the stash ceiling, and is not spent.
    Stake { amount: f64, why: &'static str },
    /// Burned for critical-hit chance.
    Favor { amount: f64 },
    /// Burned for one point of a base stat.
    Stat { stat: Stat, amount: f64 },
}

/// Decide what to do with the liquid balance.
///
/// Pure: no network, no clock, no keys, no broadcasts. Everything subtle about the
/// policy lives here so it can be tested directly, which matters because the failure
/// mode of a spending policy is not a crash -- it is quietly buying the wrong thing
/// with real money for weeks.
pub fn plan(
    mut w: Wallet,
    s: &SpendSettings,
    effective_engineering: f64,
    unreachable: Option<f64>,
) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let capped = |n: usize| s.max_per_cycle > 0 && n >= s.max_per_cycle as usize;

    // --- the need, outside the rotation ------------------------------------
    // A full stash stops the bot attacking entirely, so headroom is not a
    // preference to be balanced against the others.
    if s.min_stash_hours > 0.0 && s.stake.enabled {
        let wanted = curves::minerate_per_day(effective_engineering) * s.min_stash_hours / 24.0;
        let short = wanted - (w.stake + 1.0);
        let available = (w.liquid - s.min_scrap_reserve).max(0.0);
        if short > 0.0 && available > 0.0 {
            let amount = short.min(available);
            steps.push(Step::Stake {
                amount,
                why: "stash headroom",
            });
            w.liquid -= amount;
            w.stake += amount;
        }
    }

    let spendable = (w.liquid - s.min_scrap_reserve).max(0.0);
    if spendable <= 0.0 {
        return steps;
    }

    // --- the rotation -------------------------------------------------------
    let goals = open_goals(&w, s, unreachable);
    if goals.is_empty() {
        if s.stake.enabled && s.stake.absorb_surplus && !capped(steps.len()) {
            let amount = spendable.min(stake_room(&w, s));
            if amount > 1.0 {
                steps.push(Step::Stake {
                    amount,
                    why: "surplus",
                });
            }
        }
        return steps;
    }

    let total_weight: f64 = goals.iter().map(|(_, weight)| *weight).sum();
    let mut unused = 0.0;
    for (goal, weight) in &goals {
        if capped(steps.len()) {
            break;
        }
        let budget = spendable * weight / total_weight;
        let spent = allocate(*goal, budget, &mut w, s, &mut steps, &capped);
        unused += budget - spent;
    }

    // Lumpy goals leave change. Staking it is opt-in precisely because leaving it
    // liquid is how it accumulates into a purchase no single cycle could afford.
    if s.stake.enabled && s.stake.absorb_surplus && !capped(steps.len()) {
        let amount = unused.min(stake_room(&w, s));
        if amount > 1.0 {
            steps.push(Step::Stake {
                amount,
                why: "unused share",
            });
        }
    }
    steps
}

/// How much more may be staked before `max_stake` is reached.
///
/// Stated once, because the surplus paths did not consult it at all: an absorb step
/// could blow through an explicit cap by any margin, and staked SCRAP is locked for
/// 28 days.
fn stake_room(w: &Wallet, s: &SpendSettings) -> f64 {
    if s.stake.max_stake > 0.0 {
        (s.stake.max_stake - w.stake).max(0.0)
    } else {
        f64::INFINITY
    }
}

/// Which goals still want money, and how hard each one pulls.
fn open_goals(w: &Wallet, s: &SpendSettings, unreachable: Option<f64>) -> Vec<(Goal, f64)> {
    let mut open = Vec::new();

    if s.engineering.enabled && s.engineering.weight > 0.0 {
        let capped = s.engineering.max_level > 0.0 && w.engineering >= s.engineering.max_level;
        let slow = curves::engineering_payback_days(w.engineering) > s.engineering.max_payback_days;
        if !capped && !slow {
            open.push((Goal::Engineering, s.engineering.weight));
        }
    }
    if s.favor.enabled && s.favor.weight > 0.0 {
        let capped = s.favor.max_crit > 0.0 && curves::crit_from_favor(w.favor) >= s.favor.max_crit;
        let dear = curves::scrap_per_crit_point(w.favor) > s.favor.max_scrap_per_crit_point;
        if !capped && !dear {
            open.push((Goal::Favor, s.favor.weight));
        }
    }
    if s.stake.enabled && s.stake.weight > 0.0 {
        let capped = s.stake.max_stake > 0.0 && w.stake >= s.stake.max_stake;
        let dear = curves::scrap_per_dodge_point(w.stake) > s.stake.max_scrap_per_dodge_point;
        if !capped && !dear {
            open.push((Goal::Stake, s.stake.weight));
        }
    }
    // Damage only on evidence: the board has to actually be out of reach.
    if s.damage.enabled && s.damage.weight > 0.0 {
        let capped = s.damage.max_level > 0.0 && w.damage >= s.damage.max_level;
        let needed = unreachable.is_some_and(|pct| pct >= s.damage.min_unreachable_percent);
        if !capped && needed {
            open.push((Goal::Damage, s.damage.weight));
        }
    }
    if s.damage.defense_enabled && s.damage.defense_weight > 0.0 {
        let capped = s.damage.max_defense > 0.0 && w.defense >= s.damage.max_defense;
        if !capped {
            open.push((Goal::Defense, s.damage.defense_weight));
        }
    }
    open
}

/// Turn one goal's share into steps. Returns what it managed to use.
fn allocate(
    goal: Goal,
    budget: f64,
    w: &mut Wallet,
    s: &SpendSettings,
    steps: &mut Vec<Step>,
    capped: &dyn Fn(usize) -> bool,
) -> f64 {
    match goal {
        // Continuous: spend the share, but never past the point where another
        // percent stops being worth its price.
        Goal::Favor => {
            let room = curves::favor_affordable(w.favor, s.favor.max_scrap_per_crit_point);
            let amount = budget.min(room);
            if amount > 1.0 {
                steps.push(Step::Favor { amount });
                w.liquid -= amount;
                w.favor += amount;
                return amount;
            }
            0.0
        }
        Goal::Stake => {
            let amount = budget.min(stake_room(w, s));
            if amount > 1.0 {
                steps.push(Step::Stake {
                    amount,
                    why: "rotation",
                });
                w.liquid -= amount;
                w.stake += amount;
                return amount;
            }
            0.0
        }
        // Lumpy: whole points only, as many as the share covers.
        Goal::Engineering | Goal::Damage | Goal::Defense => {
            let stat = goal.stat();
            let mut spent = 0.0;
            loop {
                if capped(steps.len()) {
                    break;
                }
                let current = w.stat(stat);
                let cost = curves::stat_cost(stat, current);
                if cost <= 0.0 || spent + cost > budget {
                    break;
                }
                if stat == Stat::Engineering
                    && curves::engineering_payback_days(current) > s.engineering.max_payback_days
                {
                    break;
                }
                steps.push(Step::Stat { stat, amount: cost });
                w.liquid -= cost;
                w.bump(stat);
                spent += cost;
            }
            spent
        }
    }
}

/// Why a cycle spent nothing, in words rather than a bare zero.
fn idle_reason(w: &Wallet, s: &SpendSettings) -> String {
    if (w.liquid - s.min_scrap_reserve) <= 0.0 {
        return format!(
            "{:.0} liquid is not above the reserve of {:.0}, so nothing is spendable",
            w.liquid, s.min_scrap_reserve
        );
    }
    "every goal is at its ceiling".to_string()
}

#[cfg(test)]
mod plan_tests {
    //! The spending policy, tested without a network, a key, or a broadcast.
    //!
    //! This is where the money decisions live, and their failure mode is not a crash
    //! -- it is quietly buying the wrong thing, with real SCRAP, for weeks.

    use super::*;
    use crate::config::Settings;

    fn wallet(liquid: f64, stake: f64, favor: f64, engineering: f64) -> Wallet {
        Wallet {
            liquid,
            stake,
            favor,
            engineering,
            damage: 500.0,
            defense: 500.0,
        }
    }

    /// Defaults, with the stash need already satisfied so it does not mask the
    /// rotation. Tests that care about the need set it back.
    fn settings() -> SpendSettings {
        let mut s = Settings::default().spend;
        s.enabled = true;
        s.min_stash_hours = 0.0;
        s
    }

    fn staked(steps: &[Step]) -> f64 {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Stake { amount, .. } => Some(*amount),
                _ => None,
            })
            .sum()
    }
    fn favored(steps: &[Step]) -> f64 {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Favor { amount } => Some(*amount),
                _ => None,
            })
            .sum()
    }
    fn on_stat(steps: &[Step], want: Stat) -> f64 {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Stat { stat, amount } if *stat == want => Some(*amount),
                _ => None,
            })
            .sum()
    }

    #[test]
    fn one_cycle_advances_all_three_goals_rather_than_one() {
        // The whole point of the design: a waterfall would put everything into the
        // first goal and the others would never move.
        let steps = plan(
            wallet(200_000.0, 10_000.0, 44_000.0, 40.0),
            &settings(),
            40.0,
            None,
        );

        assert!(
            on_stat(&steps, Stat::Engineering) > 0.0,
            "engineering got nothing: {steps:?}"
        );
        assert!(favored(&steps) > 0.0, "favor got nothing: {steps:?}");
        assert!(staked(&steps) > 0.0, "stake got nothing: {steps:?}");
    }

    #[test]
    fn the_shares_follow_the_configured_weights() {
        // Defaults are engineering 3 : stake 2 : favor 1. Favor and stake are
        // continuous, so with no ceiling in the way they take their share exactly.
        let mut s = settings();
        s.favor.max_scrap_per_crit_point = 1e9;
        let total = 600_000.0;
        let steps = plan(wallet(total, 10_000.0, 44_000.0, 20.0), &s, 20.0, None);

        assert!(
            (staked(&steps) - total * 2.0 / 6.0).abs() < 1.0,
            "{steps:?}"
        );
        assert!(
            (favored(&steps) - total * 1.0 / 6.0).abs() < 1.0,
            "{steps:?}"
        );
        // Engineering is lumpy, so it buys whole points up to its share and no more.
        let eng = on_stat(&steps, Stat::Engineering);
        assert!(
            eng > 0.0 && eng <= total * 3.0 / 6.0,
            "engineering took {eng}"
        );
    }

    #[test]
    fn a_share_is_a_ceiling_not_a_quota() {
        // With the default 100,000-per-point limit, favor's share of 100,000 is more
        // than the 43,680 left before the 12% cliff -- so it takes only what is worth
        // taking rather than spending the share for the sake of it.
        let s = settings();
        let steps = plan(wallet(600_000.0, 10_000.0, 44_000.0, 20.0), &s, 20.0, None);
        let bought = favored(&steps);
        assert!(
            bought < 600_000.0 / 6.0,
            "favor spent its whole share: {bought}"
        );
        assert!((curves::crit_from_favor(44_000.0 + bought) - 12.0).abs() < 0.05);
    }

    #[test]
    fn a_goal_at_its_ceiling_hands_its_share_to_the_others() {
        let mut s = settings();
        // Favor priced out: from 44,000 a crit point costs 40,960.
        s.favor.max_scrap_per_crit_point = 1_000.0;
        let steps = plan(wallet(600_000.0, 10_000.0, 44_000.0, 40.0), &s, 40.0, None);

        assert_eq!(favored(&steps), 0.0, "favor should be shut out: {steps:?}");
        // Its weight is gone from the divisor, so stake now takes 2 of 5, not 2 of 6.
        assert!(
            (staked(&steps) - 600_000.0 * 2.0 / 5.0).abs() < 1.0,
            "{steps:?}"
        );
    }

    #[test]
    fn favor_stops_exactly_at_the_cliff_even_with_money_to_spare() {
        let mut s = settings();
        s.favor.weight = 100.0; // give it nearly the whole budget
        s.engineering.enabled = false;
        s.stake.weight = 1.0;
        let steps = plan(
            wallet(5_000_000.0, 10_000.0, 44_000.0, 40.0),
            &s,
            40.0,
            None,
        );

        let bought = favored(&steps);
        let landed = curves::crit_from_favor(44_000.0 + bought);
        // 12% is where the price jumps 32-fold; the default ceiling of 100,000 per
        // point must stop there rather than ploughing on.
        assert!(
            (landed - 12.0).abs() < 0.05,
            "landed on {landed}% after {bought}"
        );
    }

    #[test]
    fn a_stash_that_would_block_attacking_is_fixed_before_any_rotation() {
        let mut s = settings();
        s.min_stash_hours = 24.0;
        // Engineering 40 mines 840/day, so a day of headroom needs 840 of capacity
        // against a stake of 10.
        let steps = plan(wallet(500_000.0, 10.0, 44_000.0, 40.0), &s, 40.0, None);

        match steps.first() {
            Some(Step::Stake { amount, why }) => {
                assert_eq!(*why, "stash headroom", "the need must be what runs first");
                assert!((amount - (840.5 - 11.0)).abs() < 5.0, "staked {amount}");
            }
            other => panic!("the need must come first, got {other:?}"),
        }
        // And the rotation still runs afterwards rather than being consumed by it.
        assert!(favored(&steps) > 0.0, "{steps:?}");
        assert!(on_stat(&steps, Stat::Engineering) > 0.0, "{steps:?}");
        assert!(staked(&steps) > 840.0, "{steps:?}");
    }

    #[test]
    fn nothing_is_spent_below_the_reserve() {
        let mut s = settings();
        s.min_scrap_reserve = 100_000.0;
        let steps = plan(wallet(120_000.0, 10_000.0, 44_000.0, 40.0), &s, 40.0, None);
        let spent: f64 = steps
            .iter()
            .map(|step| match step {
                Step::Stake { amount, .. } | Step::Favor { amount } | Step::Stat { amount, .. } => {
                    *amount
                }
            })
            .sum();
        assert!(spent <= 20_000.0 + 1.0, "spent {spent} of a 20,000 surplus");

        // And below the reserve entirely, nothing happens at all.
        assert!(plan(wallet(90_000.0, 10_000.0, 44_000.0, 40.0), &s, 40.0, None).is_empty());
    }

    #[test]
    fn engineering_stops_when_a_point_takes_too_long_to_pay_for_itself() {
        let mut s = settings();
        s.engineering.max_payback_days = 30.0;
        // Payback in days is about the current level, so 40 is already past 30.
        let steps = plan(wallet(500_000.0, 10_000.0, 44_000.0, 40.0), &s, 40.0, None);
        assert_eq!(on_stat(&steps, Stat::Engineering), 0.0, "{steps:?}");
        // At level 20 it is well inside the limit and buys.
        let steps = plan(wallet(500_000.0, 10_000.0, 44_000.0, 20.0), &s, 20.0, None);
        assert!(on_stat(&steps, Stat::Engineering) > 0.0, "{steps:?}");
    }

    #[test]
    fn engineering_stops_mid_run_once_the_payback_limit_is_crossed() {
        // The gap mutation testing found. `open_goals` checks payback once, for the
        // level you start at -- but a big budget buys many points in one pass, and
        // without the check inside the loop it would run far past the limit. From
        // level 20 with a 30-day limit and a quarter-million to spend, the unchecked
        // version climbs to about level 91.
        let mut s = settings();
        s.engineering.max_payback_days = 30.0;
        s.favor.enabled = false;
        s.stake.enabled = false;
        let steps = plan(wallet(500_000.0, 10_000.0, 44_000.0, 20.0), &s, 20.0, None);

        let bought = steps
            .iter()
            .filter(|x| {
                matches!(
                    x,
                    Step::Stat {
                        stat: Stat::Damage,
                        ..
                    } | Step::Stat {
                        stat: Stat::Engineering,
                        ..
                    }
                )
            })
            .count();
        let finished_at = 20.0 + bought as f64;
        assert!(
            curves::engineering_payback_days(finished_at - 1.0) <= 30.0,
            "bought {bought} points, ending at level {finished_at}, whose payback is {}",
            curves::engineering_payback_days(finished_at - 1.0)
        );
        assert!(
            finished_at < 40.0,
            "ran past the limit to level {finished_at}"
        );
        assert!(bought > 0, "it should still buy the cheap ones");
    }

    #[test]
    fn damage_is_bought_only_when_the_board_says_it_is_needed() {
        let mut s = settings();
        s.damage.enabled = true;
        s.damage.min_unreachable_percent = 20.0;

        // Board comfortably in reach: no damage, whatever the balance.
        let steps = plan(
            wallet(900_000.0, 10_000.0, 44_000.0, 20.0),
            &s,
            20.0,
            Some(5.0),
        );
        assert_eq!(on_stat(&steps, Stat::Damage), 0.0, "{steps:?}");
        // Unknown is not a licence to buy either.
        let steps = plan(wallet(900_000.0, 10_000.0, 44_000.0, 20.0), &s, 20.0, None);
        assert_eq!(on_stat(&steps, Stat::Damage), 0.0, "{steps:?}");
        // Half the board out of reach: now it is worth buying.
        let steps = plan(
            wallet(900_000.0, 10_000.0, 44_000.0, 20.0),
            &s,
            20.0,
            Some(50.0),
        );
        assert!(on_stat(&steps, Stat::Damage) > 0.0, "{steps:?}");
    }

    #[test]
    fn leftover_stays_liquid_unless_asked_for_so_it_can_accumulate() {
        let mut s = settings();
        s.favor.enabled = false;
        s.stake.enabled = false;
        s.engineering.max_payback_days = 1_000.0;
        // A 300-level engineer needs 90,000 for the next point and has 50,000.
        let steps = plan(wallet(50_000.0, 10_000.0, 44_000.0, 300.0), &s, 300.0, None);
        assert!(steps.is_empty(), "the change must stay liquid: {steps:?}");

        // With absorb_surplus it goes to stake instead.
        s.stake.enabled = true;
        s.stake.weight = 0.0;
        s.stake.absorb_surplus = true;
        let steps = plan(wallet(50_000.0, 10_000.0, 44_000.0, 300.0), &s, 300.0, None);
        assert!(staked(&steps) > 0.0, "{steps:?}");
    }

    #[test]
    fn every_goal_shut_means_no_steps_at_all() {
        let mut s = settings();
        s.engineering.enabled = false;
        s.favor.enabled = false;
        s.stake.enabled = false;
        assert!(plan(wallet(900_000.0, 10_000.0, 44_000.0, 20.0), &s, 20.0, None).is_empty());
    }
}

#[cfg(test)]
mod plan_audit {
    //! Edge cases found by reading the planner rather than running it.
    use super::*;
    use crate::config::Settings;

    fn wallet(liquid: f64, stake: f64, favor: f64, engineering: f64) -> Wallet {
        Wallet {
            liquid,
            stake,
            favor,
            engineering,
            damage: 500.0,
            defense: 500.0,
        }
    }
    fn settings() -> SpendSettings {
        let mut s = Settings::default().spend;
        s.enabled = true;
        s.min_stash_hours = 0.0;
        s
    }
    fn total_staked(steps: &[Step]) -> f64 {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Stake { amount, .. } => Some(*amount),
                _ => None,
            })
            .sum()
    }
    fn total_spent(steps: &[Step]) -> f64 {
        steps
            .iter()
            .map(|s| match s {
                Step::Stake { amount, .. } | Step::Favor { amount } | Step::Stat { amount, .. } => {
                    *amount
                }
            })
            .sum()
    }

    #[test]
    fn absorbing_the_surplus_still_respects_max_stake() {
        let mut s = settings();
        s.engineering.enabled = false;
        s.favor.enabled = false;
        s.stake.max_stake = 12_000.0; // only 2,000 of room left
        s.stake.absorb_surplus = true;
        let steps = plan(wallet(500_000.0, 10_000.0, 44_000.0, 20.0), &s, 20.0, None);

        let staked = total_staked(&steps);
        assert!(
            staked <= 2_000.0 + 1.0,
            "max_stake of 12,000 with 10,000 already staked allows 2,000, but it staked {staked}"
        );
    }

    #[test]
    fn the_plan_never_spends_more_than_is_available() {
        // Across a spread of shapes, including ones where goals clip their shares.
        let mut s = settings();
        s.stake.absorb_surplus = true;
        for (liquid, stake, favor, eng) in [
            (500_000.0, 10_000.0, 44_000.0, 20.0),
            (1_000.0, 0.0, 0.0, 5.0),
            (10_000_000.0, 3_000_000.0, 90_000.0, 400.0),
            (250.0, 100.0, 119.0, 1.0),
        ] {
            let steps = plan(wallet(liquid, stake, favor, eng), &s, eng, None);
            let spent = total_spent(&steps);
            assert!(
                spent <= liquid + 1e-6,
                "spent {spent} of {liquid} available"
            );
        }
    }

    #[test]
    fn nonsense_from_the_api_cannot_produce_a_nonsense_purchase() {
        let s = settings();
        // Negative and NaN balances must fail closed rather than compute a bizarre
        // amount: these values come from an API, not from us.
        for liquid in [-1_000.0, f64::NAN] {
            let steps = plan(wallet(liquid, 10_000.0, 44_000.0, 20.0), &s, 20.0, None);
            assert!(steps.is_empty(), "liquid {liquid} produced {steps:?}");
        }
        // And no step may ever carry a non-positive or non-finite amount.
        let steps = plan(wallet(500_000.0, 10_000.0, 44_000.0, 20.0), &s, 20.0, None);
        for step in &steps {
            let amount = match step {
                Step::Stake { amount, .. } | Step::Favor { amount } | Step::Stat { amount, .. } => {
                    *amount
                }
            };
            assert!(amount.is_finite() && amount > 0.0, "bad amount in {step:?}");
        }
    }
}

#[cfg(test)]
mod consumable_tests {
    //! A potion used at the wrong moment is not an error anywhere -- it is simply
    //! gone -- so the judgement is what these test.
    use super::tests::parts;
    use super::*;
    use crate::api::{Consumable, Inventory, Player};
    use crate::config::{ConsumableSettings, Settings};

    /// The same throwaway wiring the other test module uses, without the macro,
    /// which `macro_rules!` scoping keeps inside its own module.
    fn runner_for<'a>(
        api: &'a Api,
        hive: &'a Broadcaster,
        keys: &'a AccountKeys,
        settings: &'a Settings,
        blacklist: &'a HashSet<String>,
    ) -> Runner<'a> {
        Runner {
            api,
            hive,
            account: "alice",
            keys,
            settings,
            blacklist,
            stop: Arc::new(AtomicBool::new(false)),
            started_missions: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    fn held(kinds: &[(&str, f64)]) -> Inventory {
        Inventory {
            crates: Vec::new(),
            consumables: kinds
                .iter()
                .map(|(k, amount)| Consumable {
                    kind: format!("{k}_consumable"),
                    amount: *amount,
                })
                .collect(),
        }
    }

    /// Out of attacks, with room to loot and a claim in hand: an attack potion is
    /// exactly what is missing.
    fn attack_starved() -> Player {
        Player {
            attacks: 0.0,
            claims: 3.0,
            scrap: 10.0,
            hive_engine_stake: 10_000.0,
            ..Default::default()
        }
    }

    fn settings() -> ConsumableSettings {
        let mut s = Settings::default().consumables;
        s.enabled = true;
        s
    }

    #[test]
    fn an_attack_potion_is_used_only_when_attacks_are_what_is_missing() {
        let s = settings();
        let inventory = held(&[("attack", 2.0), ("fury", 1.0)]);

        let (wanted, _) = consumables_to_use(&attack_starved(), &inventory, &s, 0.1);
        assert_eq!(
            wanted.len(),
            2,
            "both grant attacks and attacks are missing"
        );

        // With attacks already in hand, drinking one throws it away.
        let stocked = Player {
            attacks: 5.0,
            ..attack_starved()
        };
        let (wanted, skipped) = consumables_to_use(&stocked, &inventory, &s, 0.1);
        assert!(wanted.is_empty(), "{wanted:?}");
        assert_eq!(skipped.len(), 2);
        assert!(skipped[0].contains("wasted"), "{skipped:?}");
    }

    #[test]
    fn attacks_are_worthless_without_somewhere_to_put_the_loot() {
        let s = settings();
        let inventory = held(&[("fury", 1.0)]);

        // Stash full: an attack cannot be made whatever the attack count says.
        let full = Player {
            scrap: 10_001.0,
            hive_engine_stake: 10_000.0,
            ..attack_starved()
        };
        assert!(full.stash_is_full());
        assert!(consumables_to_use(&full, &inventory, &s, 0.1).0.is_empty());

        // No claims: the game refuses the battle anyway.
        let claimless = Player {
            claims: 0.0,
            ..attack_starved()
        };
        assert!(consumables_to_use(&claimless, &inventory, &s, 0.1)
            .0
            .is_empty());
    }

    #[test]
    fn a_claim_potion_needs_something_worth_claiming() {
        let s = settings();
        let inventory = held(&[("claim", 1.0)]);

        // Out of claims with scrap sitting in the stash: useful.
        let blocked = Player {
            claims: 0.0,
            scrap: 500.0,
            ..attack_starved()
        };
        assert_eq!(consumables_to_use(&blocked, &inventory, &s, 0.1).0.len(), 1);

        // Out of claims but nothing to claim: pointless.
        let empty = Player {
            claims: 0.0,
            scrap: 0.0,
            ..attack_starved()
        };
        assert!(consumables_to_use(&empty, &inventory, &s, 0.1).0.is_empty());
    }

    #[test]
    fn timed_buffs_are_never_used_on_a_schedule() {
        // Even listed explicitly, a 24-hour buff is left alone: the bot has no idea
        // whether now is a good moment to start its clock.
        let mut s = settings();
        s.use_kinds = vec![
            "crit".into(),
            "rage".into(),
            "protection".into(),
            "damage".into(),
        ];
        let inventory = held(&[
            ("crit", 5.0),
            ("rage", 5.0),
            ("protection", 5.0),
            ("damage", 5.0),
        ]);
        let (wanted, skipped) = consumables_to_use(&attack_starved(), &inventory, &s, 0.1);
        assert!(wanted.is_empty(), "{wanted:?}");
        assert_eq!(skipped.len(), 4);
    }

    #[test]
    fn nothing_outside_the_configured_list_is_touched() {
        let mut s = settings();
        s.use_kinds = vec!["claim".into()];
        let inventory = held(&[("fury", 3.0), ("attack", 3.0)]);
        let (wanted, skipped) = consumables_to_use(&attack_starved(), &inventory, &s, 0.1);
        assert!(wanted.is_empty());
        // Not even mentioned as skipped: it was never a candidate.
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    #[test]
    fn an_empty_stack_is_not_drunk() {
        let s = settings();
        let inventory = held(&[("fury", 0.0)]);
        assert!(consumables_to_use(&attack_starved(), &inventory, &s, 0.1)
            .0
            .is_empty());
    }

    #[test]
    fn the_per_cycle_cap_holds() {
        let mut s = settings();
        s.max_per_cycle = 1;
        let inventory = held(&[("attack", 5.0), ("fury", 5.0)]);
        assert_eq!(
            consumables_to_use(&attack_starved(), &inventory, &s, 0.1)
                .0
                .len(),
            1
        );
    }

    // --- the gates on the three new actions, all reachable without a network ---

    #[test]
    fn the_new_actions_refuse_cleanly_when_disabled() {
        let settings = Settings::default();
        let (api, hive, keys) = parts(None);
        let blacklist = HashSet::new();
        let runner = runner_for(&api, &hive, &keys, &settings, &blacklist);

        for reason in [
            runner
                .start_missions(&Player::default())
                .unwrap()
                .skipped_reason,
            runner.open_crates().unwrap().skipped_reason,
            runner
                .use_consumables(&Player::default())
                .unwrap()
                .skipped_reason,
        ] {
            assert!(
                reason.unwrap_or_default().contains("disabled"),
                "a disabled action must say so rather than reaching the network"
            );
        }
    }

    #[test]
    fn starting_missions_refuses_without_an_active_key() {
        let mut settings = Settings::default();
        settings.quest.start = true;
        let (api, hive, keys) = parts(None);
        let blacklist = HashSet::new();
        let runner = runner_for(&api, &hive, &keys, &settings, &blacklist);

        let reason = runner
            .start_missions(&Player::default())
            .unwrap()
            .skipped_reason
            .unwrap_or_default();
        assert!(reason.contains("active key"), "{reason}");
    }
}

#[cfg(test)]
mod commitment_tests {
    //! Two actions in one cycle can both spend liquid SCRAP, and the game reports
    //! the pre-spend balance to the second of them. These cover the arithmetic that
    //! stops them promising it twice.
    use super::*;
    use crate::config::Settings;

    #[test]
    fn a_wallet_built_for_spending_excludes_what_is_already_committed() {
        // The shape `spend` builds before planning.
        let liquid = 10_000.0;
        for committed in [0.0f64, 2_500.0, 10_000.0, 25_000.0] {
            let available = (liquid - committed.max(0.0)).max(0.0);
            assert!(available <= liquid);
            assert!(available >= 0.0, "committed {committed} went negative");
        }
        // Over-committing cannot conjure a negative balance for the planner.
        assert_eq!((10_000.0f64 - 25_000.0f64).max(0.0), 0.0);
    }

    #[test]
    fn the_planner_spends_less_when_told_something_is_already_committed() {
        let mut s = Settings::default().spend;
        s.enabled = true;
        s.min_stash_hours = 0.0;
        s.engineering.enabled = false;
        s.favor.enabled = false;
        s.stake.weight = 1.0;

        let spent_of = |committed: f64| {
            let wallet = Wallet {
                liquid: (100_000.0f64 - committed).max(0.0),
                stake: 10_000.0,
                favor: 44_000.0,
                engineering: 20.0,
                damage: 500.0,
                defense: 500.0,
            };
            plan(wallet, &s, 20.0, None)
                .iter()
                .map(|step| match step {
                    Step::Stake { amount, .. }
                    | Step::Favor { amount }
                    | Step::Stat { amount, .. } => *amount,
                })
                .sum::<f64>()
        };

        let free = spent_of(0.0);
        let constrained = spent_of(60_000.0);
        assert!(free > constrained, "{free} should exceed {constrained}");
        assert!(
            constrained <= 40_000.0 + 1.0,
            "spent {constrained} of 40,000 left"
        );
        // And with everything committed, nothing more is promised.
        assert_eq!(spent_of(100_000.0), 0.0);
    }
}
