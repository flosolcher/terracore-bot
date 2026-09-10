//! Choosing who to attack.
//!
//! The rules are the game client's own, ported one for one, because the client is the
//! authority on what the server will accept. Attacking a target the client would have
//! filtered out spends an attack for nothing:
//!
//! * a name containing `_` or `!` is not a real player row,
//! * yourself is not a target,
//! * a target with no scrap has nothing to take,
//! * a defense at or above your damage loses, unless a focus charge is spent,
//! * a target battled in the last minute is briefly untouchable,
//! * an account registered in the last 24 hours is off limits,
//! * an active protection consumable makes a target immune for 24 hours.
//!
//! On top of those the bot adds its own: a blacklist, a dodge ceiling, a minimum
//! worth attacking, and a margin on the damage comparison.
//!
//! Ranking is the client's too: scrap discounted by the dodge chance. A dodged attack
//! still spends the attack, so a rich target that dodges half the time is worth half.

use std::collections::HashSet;

use crate::api::{Millis, Target};
use crate::config::AttackSettings;

/// A target battled this recently is skipped. From the client: 60 seconds.
pub const BATTLE_COOLDOWN_MS: Millis = 60_000;
/// New accounts are off limits for their first day. From the client: 24 hours.
pub const NEW_PLAYER_GRACE_MS: Millis = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    NotAPlayerRow,
    Yourself,
    NoScrap,
    BelowMinimumScrap,
    OutOfReach,
    DodgesTooOften,
    RecentlyBattled,
    TooNew,
    Protected,
    Blacklisted,
}

impl Rejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Rejection::NotAPlayerRow => "not a player row",
            Rejection::Yourself => "yourself",
            Rejection::NoScrap => "no scrap to take",
            Rejection::BelowMinimumScrap => "below min_target_scrap",
            Rejection::OutOfReach => "defense too high",
            Rejection::DodgesTooOften => "above max_enemy_dodge",
            Rejection::RecentlyBattled => "battled in the last minute",
            Rejection::TooNew => "registered less than a day ago",
            Rejection::Protected => "protection consumable active",
            Rejection::Blacklisted => "blacklisted",
        }
    }
}

/// Everything the decision depends on, gathered so the rule itself stays testable
/// without a network or a clock.
pub struct Context<'a> {
    pub me: &'a str,
    /// Effective damage, items and buffs included.
    pub my_damage: f64,
    /// Focus charges held. Each one buys one attack against an over-defended target.
    pub focus_charges: u32,
    pub settings: &'a AttackSettings,
    /// Lowercased account names never to attack.
    pub blacklist: &'a HashSet<String>,
    pub now: Millis,
}

impl Context<'_> {
    /// Whether the defense rule may be ignored, which the client allows only while a
    /// focus charge is held *and* the focus board was the one fetched.
    pub fn focus_active(&self) -> bool {
        self.settings.use_focus && self.focus_charges > 0
    }
}

/// Why a single target was refused, or `None` if it is fair game.
pub fn reject(target: &Target, ctx: &Context) -> Option<Rejection> {
    let name = target.username.to_ascii_lowercase();

    if name.is_empty() || name.contains('_') || name.contains('!') {
        return Some(Rejection::NotAPlayerRow);
    }
    if name == ctx.me.to_ascii_lowercase() {
        return Some(Rejection::Yourself);
    }
    if ctx.blacklist.contains(&name) {
        return Some(Rejection::Blacklisted);
    }
    if target.scrap <= 0.0 {
        return Some(Rejection::NoScrap);
    }
    if target.scrap < ctx.settings.min_target_scrap {
        return Some(Rejection::BelowMinimumScrap);
    }
    if !ctx.focus_active() && target.defense() + ctx.settings.min_damage_margin >= ctx.my_damage {
        return Some(Rejection::OutOfReach);
    }
    if target.dodge() > ctx.settings.max_enemy_dodge {
        return Some(Rejection::DodgesTooOften);
    }
    if target.last_battle > ctx.now - BATTLE_COOLDOWN_MS {
        return Some(Rejection::RecentlyBattled);
    }
    if target.registration_time > ctx.now - NEW_PLAYER_GRACE_MS {
        return Some(Rejection::TooNew);
    }
    if target.consumables.protection_remaining_ms(ctx.now) > 0 {
        return Some(Rejection::Protected);
    }
    None
}

/// Every eligible target, richest expected haul first.
pub fn rank(targets: &[Target], ctx: &Context) -> Vec<Target> {
    let mut eligible: Vec<Target> = targets
        .iter()
        .filter(|t| reject(t, ctx).is_none())
        .cloned()
        .collect();
    eligible.sort_by(|a, b| {
        b.expected_scrap()
            .partial_cmp(&a.expected_scrap())
            // NaN cannot come out of the API, but a total order costs nothing and
            // `sort_by` with a partial one would panic if it ever did.
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    eligible
}

/// A tally of why the board emptied out, for the log line after a cycle finds
/// nothing. "No opponent found" on its own is not a diagnosis.
pub fn tally(targets: &[Target], ctx: &Context) -> Vec<(Rejection, usize)> {
    let order = [
        Rejection::OutOfReach,
        Rejection::Protected,
        Rejection::RecentlyBattled,
        Rejection::DodgesTooOften,
        Rejection::NoScrap,
        Rejection::BelowMinimumScrap,
        Rejection::Blacklisted,
        Rejection::TooNew,
        Rejection::Yourself,
        Rejection::NotAPlayerRow,
    ];
    let reasons: Vec<Rejection> = targets.iter().filter_map(|t| reject(t, ctx)).collect();
    order
        .into_iter()
        .filter_map(|r| {
            let n = reasons.iter().filter(|&&x| x == r).count();
            (n > 0).then_some((r, n))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Consumables, Stats};

    const NOW: Millis = 1_700_000_000_000;

    fn settings() -> AttackSettings {
        AttackSettings {
            max_enemy_dodge: 50.0,
            min_target_scrap: 1.0,
            min_damage_margin: 0.0,
            use_focus: false,
            ..Default::default()
        }
    }

    fn target(name: &str, scrap: f64, defense: f64, dodge: f64) -> Target {
        Target {
            username: name.into(),
            scrap,
            stats: Stats {
                defense,
                dodge,
                ..Default::default()
            },
            // Old enough and idle enough to pass the time-based rules by default.
            registration_time: NOW - 90 * 24 * 60 * 60 * 1000,
            last_battle: NOW - 10 * 60 * 1000,
            ..Default::default()
        }
    }

    fn ctx<'a>(s: &'a AttackSettings, blacklist: &'a HashSet<String>) -> Context<'a> {
        Context {
            me: "alice",
            my_damage: 500.0,
            focus_charges: 0,
            settings: s,
            blacklist,
            now: NOW,
        }
    }

    #[test]
    fn a_reachable_target_is_accepted() {
        let (s, b) = (settings(), HashSet::new());
        assert_eq!(reject(&target("bob", 100.0, 400.0, 10.0), &ctx(&s, &b)), None);
    }

    #[test]
    fn equal_defense_loses_so_it_is_refused() {
        let (s, b) = (settings(), HashSet::new());
        // The client's rule is `defense >= damage`, not `>`.
        assert_eq!(
            reject(&target("bob", 100.0, 500.0, 0.0), &ctx(&s, &b)),
            Some(Rejection::OutOfReach)
        );
        assert_eq!(reject(&target("bob", 100.0, 499.0, 0.0), &ctx(&s, &b)), None);
    }

    #[test]
    fn a_margin_widens_the_gap_that_is_required() {
        let mut s = settings();
        s.min_damage_margin = 50.0;
        let b = HashSet::new();
        assert_eq!(
            reject(&target("bob", 100.0, 470.0, 0.0), &ctx(&s, &b)),
            Some(Rejection::OutOfReach)
        );
        assert_eq!(reject(&target("bob", 100.0, 440.0, 0.0), &ctx(&s, &b)), None);
    }

    #[test]
    fn focus_lifts_the_defense_rule_but_only_when_a_charge_is_held() {
        let mut s = settings();
        s.use_focus = true;
        let b = HashSet::new();

        let mut c = ctx(&s, &b);
        c.focus_charges = 0;
        assert_eq!(
            reject(&target("bob", 100.0, 9000.0, 0.0), &c),
            Some(Rejection::OutOfReach)
        );

        c.focus_charges = 1;
        assert_eq!(reject(&target("bob", 100.0, 9000.0, 0.0), &c), None);
    }

    #[test]
    fn the_time_based_rules_match_the_client() {
        let (s, b) = (settings(), HashSet::new());

        let mut fresh = target("bob", 100.0, 100.0, 0.0);
        fresh.last_battle = NOW - 59_000;
        assert_eq!(reject(&fresh, &ctx(&s, &b)), Some(Rejection::RecentlyBattled));
        fresh.last_battle = NOW - 61_000;
        assert_eq!(reject(&fresh, &ctx(&s, &b)), None);

        let mut newbie = target("bob", 100.0, 100.0, 0.0);
        newbie.registration_time = NOW - 23 * 60 * 60 * 1000;
        assert_eq!(reject(&newbie, &ctx(&s, &b)), Some(Rejection::TooNew));
    }

    #[test]
    fn protection_and_the_blacklist_both_shield_a_target() {
        let s = settings();
        let mut protected = target("bob", 100.0, 100.0, 0.0);
        protected.consumables = Consumables {
            protection: 1.0,
            protection_times: vec![(NOW - 1000) as f64],
            focus: 0.0,
        };
        assert_eq!(
            reject(&protected, &ctx(&s, &HashSet::new())),
            Some(Rejection::Protected)
        );

        let blacklist: HashSet<String> = ["bob".to_string()].into_iter().collect();
        assert_eq!(
            reject(&target("BoB", 100.0, 100.0, 0.0), &ctx(&s, &blacklist)),
            Some(Rejection::Blacklisted)
        );
    }

    #[test]
    fn ranking_discounts_scrap_by_the_dodge_chance() {
        let (s, b) = (settings(), HashSet::new());
        let targets = vec![
            // 1000 scrap but dodges half the time: worth 500.
            target("dodger", 1000.0, 100.0, 50.0),
            // 800 scrap and never dodges: worth 800, so it wins.
            target("sitting-duck", 800.0, 100.0, 0.0),
            target("poor", 10.0, 100.0, 0.0),
        ];
        let ranked = rank(&targets, &ctx(&s, &b));
        let names: Vec<&str> = ranked.iter().map(|t| t.username.as_str()).collect();
        assert_eq!(names, ["sitting-duck", "dodger", "poor"]);
    }

    #[test]
    fn the_tally_explains_an_empty_board() {
        let (s, b) = (settings(), HashSet::new());
        let targets = vec![
            target("tough1", 100.0, 900.0, 0.0),
            target("tough2", 100.0, 900.0, 0.0),
            target("spinner", 100.0, 100.0, 99.0),
        ];
        let ctx = ctx(&s, &b);
        assert!(rank(&targets, &ctx).is_empty());
        assert_eq!(
            tally(&targets, &ctx),
            vec![(Rejection::OutOfReach, 2), (Rejection::DodgesTooOften, 1)]
        );
    }
}
