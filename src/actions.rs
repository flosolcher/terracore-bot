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
use crate::config::{BossOrder, Settings, Stat, UpgradeOrder};
use crate::hive::{tx_hash, Auth, Broadcaster};
use crate::keys::AccountKeys;
use crate::targeting::{self, Context as TargetContext};

/// Ports the website's upgrade pricing. `engineering` costs its own current value
/// squared; `damage` and `defense` cost a tenth of theirs, squared. The basis is the
/// *base* stat, before items -- equipping a better weapon does not make the next
/// point of damage cheaper.
pub fn upgrade_cost(stat: Stat, current: f64) -> f64 {
    match stat {
        Stat::Engineering => current * current,
        Stat::Damage | Stat::Defense => (current / 10.0) * (current / 10.0),
    }
}

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
}

/// What one action did, for the cycle summary.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub performed: u32,
    pub skipped_reason: Option<String>,
}

impl Outcome {
    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            performed: 0,
            skipped_reason: Some(reason.into()),
        }
    }

    fn did(n: u32) -> Self {
        Self {
            performed: n,
            skipped_reason: None,
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
    // Upgrades -- active key
    // -----------------------------------------------------------------------

    /// Burn liquid SCRAP to raise a base stat.
    ///
    /// Paid from the Hive-Engine balance, not the stash: unclaimed scrap cannot buy
    /// anything, which is why claiming comes first in a cycle.
    pub fn upgrades(&self, player: &Player) -> Result<Outcome> {
        let s = &self.settings.upgrade;
        if !s.enabled {
            return Ok(Outcome::skipped("upgrades are disabled"));
        }
        let active = match self.active_key("upgrades") {
            Ok(key) => key,
            Err(outcome) => return Ok(outcome),
        };

        // Tracked locally so several upgrades in one cycle each pay the right,
        // rising price -- the API will not have caught up between broadcasts.
        let mut engineering = player.engineering;
        let mut damage = player.damage;
        let mut defense = player.defense;
        let mut balance = player.hive_engine_scrap;
        let mut done = 0;
        let mut last_reason = String::new();

        while s.max_per_cycle == 0 || done < s.max_per_cycle {
            if self.stopping() {
                break;
            }

            let current = |stat: Stat| match stat {
                Stat::Engineering => engineering,
                Stat::Damage => damage,
                Stat::Defense => defense,
            };
            let ceiling = |stat: Stat| match stat {
                Stat::Engineering => s.max_engineering,
                Stat::Damage => s.max_damage,
                Stat::Defense => s.max_defense,
            };

            let mut affordable: Vec<(Stat, f64)> = s
                .stats
                .iter()
                .copied()
                .filter_map(|stat| {
                    let cap = ceiling(stat);
                    if cap > 0.0 && current(stat) >= cap {
                        last_reason = format!("{} is at its configured ceiling", stat.as_str());
                        return None;
                    }
                    let cost = upgrade_cost(stat, current(stat));
                    if s.max_cost > 0.0 && cost > s.max_cost {
                        last_reason = format!(
                            "the next {} costs {:.0}, above max_cost",
                            stat.as_str(),
                            cost
                        );
                        return None;
                    }
                    if balance - cost < s.min_scrap_reserve {
                        last_reason = format!(
                            "the next {} costs {:.0}, leaving less than min_scrap_reserve of {:.0}",
                            stat.as_str(),
                            cost,
                            s.min_scrap_reserve
                        );
                        return None;
                    }
                    Some((stat, cost))
                })
                .collect();

            if s.order == UpgradeOrder::Cheapest {
                affordable
                    .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            }

            let Some(&(stat, cost)) = affordable.first() else {
                break;
            };

            let sent = self.hive.engine_transfer(
                self.account,
                active,
                "SCRAP",
                "null",
                &format_number(cost),
                json!(format!("terracore_{}-{}", stat.as_str(), tx_hash())),
            )?;
            info!(
                account = self.account,
                stat = stat.as_str(),
                from = current(stat),
                cost = format!("{:.2}", cost),
                trx = sent.trx_id(),
                dry_run = sent.was_dry_run(),
                "upgraded",
            );

            balance -= cost;
            match stat {
                Stat::Engineering => engineering += 1.0,
                Stat::Damage => damage += 1.0,
                Stat::Defense => defense += 1.0,
            }
            done += 1;
            self.wait(s.delay_secs);
        }

        if done == 0 {
            return Ok(Outcome::skipped(if last_reason.is_empty() {
                "nothing to upgrade".to_string()
            } else {
                last_reason
            }));
        }
        Ok(Outcome::did(done))
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
    fn parts(active: Option<PrivateKey>) -> (Api, Broadcaster, AccountKeys) {
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
            }
        };
    }

    #[test]
    fn boss_fights_and_upgrades_refuse_to_run_without_an_active_key() {
        let mut settings = Settings::default();
        settings.boss.enabled = true;
        settings.upgrade.enabled = true;

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

        let upgrade = runner.upgrades(&Player::default()).unwrap();
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
        settings.upgrade.enabled = true;

        let (api, hive, keys) = parts(Some(PrivateKey::from_wif(THROWAWAY).unwrap()));
        let blacklist = HashSet::new();
        let runner = runner!(api, hive, keys, settings, blacklist);

        // A stat worth 10,000 SCRAP to raise, and nothing in the wallet to pay with,
        // so it stops for lack of funds rather than for lack of a key.
        let player = Player {
            engineering: 100.0,
            hive_engine_scrap: 0.0,
            ..Default::default()
        };
        let upgrade = runner.upgrades(&player).unwrap();
        assert_eq!(upgrade.performed, 0);
        let reason = upgrade.skipped_reason.unwrap_or_default();
        assert!(!reason.contains("active key"), "{reason}");
        assert!(reason.contains("min_scrap_reserve"), "{reason}");
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

    #[test]
    fn upgrade_prices_match_the_website() {
        // engineering: n^2. A level-519 engineer pays 519^2 for the next point.
        assert_eq!(upgrade_cost(Stat::Engineering, 519.0), 269_361.0);
        // damage and defense: (n/10)^2.
        assert_eq!(upgrade_cost(Stat::Damage, 3290.0), 108_241.0);
        assert_eq!(upgrade_cost(Stat::Defense, 100.0), 100.0);
        // Fractional stats price fractionally rather than rounding, which is what
        // the client's arithmetic does.
        assert_eq!(upgrade_cost(Stat::Damage, 3295.0), 108_570.25);
    }

    #[test]
    fn the_cost_curve_rises_with_the_stat() {
        assert!(upgrade_cost(Stat::Engineering, 100.0) < upgrade_cost(Stat::Engineering, 101.0));
    }
}
