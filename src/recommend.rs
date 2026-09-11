//! Settings suggested from an account's own numbers.
//!
//! Not a fixed preset. What a sensible ceiling is depends entirely on where the
//! account already stands: a crit point costing 40,960 is a bargain for someone
//! mining 95,000 a day and absurd for someone mining 200.
//!
//! One rule does most of the work:
//!
//! > keep buying while one more point costs less than a day of mining income.
//!
//! It self-scales -- as income grows the ceilings rise with it -- and because every
//! curve in this game walls exponentially, it stops at the next cliff on its own
//! rather than at a percentage that goes stale.

use serde::Serialize;

use crate::api::Player;
use crate::config::SpendSettings;
use crate::curves;

/// One suggested value, and why.
///
/// The reason travels with the number on purpose: a recommendation a person cannot
/// interrogate is just a magic constant with better marketing.
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub field: String,
    pub value: String,
    pub why: String,
}

/// Engineering past this payback is not worth buying at any weight.
const HOPELESS_PAYBACK_DAYS: f64 = 365.0;
/// What a point of anything may cost, as a multiple of daily mining income.
const BUDGET_DAYS_PER_POINT: f64 = 1.0;

/// Suggest spend settings for this account, preserving the choices this has no
/// opinion about (whether spending is on at all, delays, per-cycle caps).
pub fn spend_settings(player: &Player, current: &SpendSettings) -> (SpendSettings, Vec<Note>) {
    let mut s = current.clone();
    let mut notes = Vec::new();

    let per_day = curves::minerate_per_day(player.stats.engineering);
    let budget_per_point = (per_day * BUDGET_DAYS_PER_POINT).max(1.0);

    // --- the need -------------------------------------------------------
    s.min_stash_hours = 24.0;
    notes.push(Note {
        field: "min_stash_hours".into(),
        value: "24".into(),
        why: format!(
            "a day of downtime without overflowing: you mine about {per_day:.0} SCRAP a day, \
             so the stash needs that much headroom or attacking stops"
        ),
    });

    // A working buffer. Staked SCRAP takes 28 days to come back, so leaving a day's
    // income liquid is the difference between having options and not.
    s.min_scrap_reserve = round_nicely(per_day);
    notes.push(Note {
        field: "min_scrap_reserve".into(),
        value: format!("{:.0}", s.min_scrap_reserve),
        why: "about a day of mining kept liquid, because staked SCRAP needs 28 days to \
              unstake and burned SCRAP never comes back"
            .into(),
    });

    // --- engineering ----------------------------------------------------
    //
    // The ceiling is not a fixed number of days. Engineering is normally the
    // cheapest point on offer -- `e^2` against tens of thousands for a crit or dodge
    // point -- and it stops deserving priority when it costs what the alternatives
    // do. Solving `e^2 = cheapest alternative` gives the level where that happens,
    // and its payback is the limit worth setting.
    //
    // A flat 60 looked reasonable and was not: an account at engineering 75 already
    // has a 74-day payback, so 60 shut out the one goal it could actually afford and
    // left nothing enabled at all.
    let crit_point = curves::scrap_per_crit_point(player.favor);
    let dodge_point = curves::scrap_per_dodge_point(player.hive_engine_stake);
    let cheapest_alternative = crit_point.min(dodge_point);
    let crossover = if cheapest_alternative.is_finite() {
        cheapest_alternative.max(1.0).sqrt()
    } else {
        f64::from(u16::MAX)
    };
    let target_days = curves::engineering_payback_days(crossover).clamp(60.0, 365.0);

    let payback = curves::engineering_payback_days(player.engineering);
    if payback > HOPELESS_PAYBACK_DAYS {
        s.engineering.enabled = false;
        notes.push(Note {
            field: "engineering.enabled".into(),
            value: "false".into(),
            why: format!(
                "at engineering {:.0} another point takes {payback:.0} days to mine back its \
                 own cost, which is past the point of being worth it",
                player.engineering
            ),
        });
    } else {
        s.engineering.enabled = true;
        s.engineering.max_payback_days = round_nicely(target_days).max(60.0);
        notes.push(Note {
            field: "engineering.max_payback_days".into(),
            value: format!("{:.0}", s.engineering.max_payback_days),
            why: format!(
                "a point costs {:.0} now and repays in {payback:.0} days. Below the 333 softcap \
                 the payback in days is about your level, so this keeps buying to roughly \
                 engineering {:.0} -- the point where a point of engineering costs what a point \
                 of crit or dodge costs you today ({cheapest_alternative:.0})",
                curves::stat_cost(crate::config::Stat::Engineering, player.engineering),
                s.engineering.max_payback_days,
            ),
        });
    }

    // --- favor ----------------------------------------------------------
    let crit_now = curves::crit_from_favor(player.favor);
    s.favor.max_scrap_per_crit_point = round_nicely(budget_per_point);
    if crit_point <= budget_per_point {
        let room = curves::favor_affordable(player.favor, s.favor.max_scrap_per_crit_point);
        let landing = curves::crit_from_favor(player.favor + room);
        notes.push(Note {
            field: "favor.max_scrap_per_crit_point".into(),
            value: format!("{:.0}", s.favor.max_scrap_per_crit_point),
            why: format!(
                "you are at {crit_now:.3}% crit and a point costs {crit_point:.0}, under a day \
                 of mining -- this buys up to about {landing:.1}% and then stops at the cliff"
            ),
        });
    } else {
        notes.push(Note {
            field: "favor.max_scrap_per_crit_point".into(),
            value: format!("{:.0}", s.favor.max_scrap_per_crit_point),
            why: format!(
                "you are at {crit_now:.3}% crit, where a point already costs {crit_point:.0} -- \
                 more than a day of mining, so this leaves favor alone until income catches up"
            ),
        });
    }

    // --- stake ----------------------------------------------------------
    s.stake.max_scrap_per_dodge_point = round_nicely(budget_per_point);
    s.stake.absorb_surplus = false;
    notes.push(Note {
        field: "stake.max_scrap_per_dodge_point".into(),
        value: format!("{:.0}", s.stake.max_scrap_per_dodge_point),
        why: format!(
            "dodge is at {:.2}% and a point costs {dodge_point:.0}; staking is not spending, \
             but it is locked for 28 days, so it stops once a point costs over a day of mining",
            curves::dodge_from_stake(player.hive_engine_stake)
        ),
    });
    notes.push(Note {
        field: "stake.absorb_surplus".into(),
        value: "false".into(),
        why: "leftovers stay liquid so they accumulate into the next purchase, rather than \
              disappearing into a 28-day lock every cycle"
            .into(),
    });

    // --- weights --------------------------------------------------------
    s.engineering.weight = 3.0;
    s.stake.weight = 2.0;
    s.favor.weight = 1.0;
    notes.push(Note {
        field: "weights".into(),
        value: "engineering 3, stake 2, favor 1".into(),
        why: "engineering is the only goal that compounds; staking is not spent, so it is \
              cheap; favor needs only a trickle because its price doubles at every band"
            .into(),
    });

    (s, notes)
}

/// Round to something a person would have typed.
fn round_nicely(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 0.0;
    }
    let magnitude = 10f64.powf(value.log10().floor() - 1.0).max(1.0);
    (value / magnitude).round() * magnitude
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Stats;

    fn player(engineering: f64, favor: f64, stake: f64) -> Player {
        Player {
            engineering,
            favor,
            hive_engine_stake: stake,
            stats: Stats {
                engineering,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn an_established_account_is_told_to_finish_the_cheap_crit() {
        // The real shape this was designed against: 44,000 favor at 10.933% crit,
        // where a point costs 40,960 and the cliff is at 12%.
        let (s, notes) = spend_settings(
            &player(531.0, 44_000.0, 2_900_000.0),
            &SpendSettings::default(),
        );

        assert!(
            s.favor.max_scrap_per_crit_point > 40_960.0,
            "a crit point costs 40,960 and income covers it, so the ceiling must clear it: {}",
            s.favor.max_scrap_per_crit_point
        );
        // And the ceiling must stop before the 32x cliff rather than ploughing through.
        assert!(s.favor.max_scrap_per_crit_point < 1_310_720.0);
        let room = curves::favor_affordable(44_000.0, s.favor.max_scrap_per_crit_point);
        assert!((curves::crit_from_favor(44_000.0 + room) - 12.0).abs() < 0.05);

        // Engineering at 531 pays back in ~1300 days, so it is not worth buying.
        assert!(!s.engineering.enabled);
        assert!(notes.iter().any(|n| n.field == "engineering.enabled"));
    }

    /// The account this was actually applied to, and the case that showed the flat
    /// 60-day ceiling was wrong: engineering 75 has a 74-day payback, so a limit of
    /// 60 closed the only goal it could afford and left nothing enabled at all.
    #[test]
    fn a_mid_account_is_not_left_with_every_goal_shut() {
        let mut p = player(75.0, 44_000.0, 43_999.0);
        p.hive_engine_scrap = 14_743.0;
        let (s, _) = spend_settings(&p, &SpendSettings::default());

        assert!(s.engineering.enabled);
        assert!(
            s.engineering.max_payback_days > curves::engineering_payback_days(75.0),
            "the ceiling must leave room to buy: {} vs a payback of {:.0}",
            s.engineering.max_payback_days,
            curves::engineering_payback_days(75.0)
        );

        // And the whole thing must actually do something: at least one goal open.
        let mut spend = s.clone();
        spend.enabled = true;
        let wallet = crate::actions::Wallet {
            liquid: 14_743.0,
            stake: 43_999.0,
            favor: 44_000.0,
            engineering: 75.0,
            damage: 1294.0,
            defense: 760.0,
        };
        let steps = crate::actions::plan(wallet, &spend, 75.0, None);
        assert!(
            !steps.is_empty(),
            "the suggested config does nothing at all"
        );
    }

    #[test]
    fn a_young_account_is_told_to_buy_engineering() {
        let (s, _) = spend_settings(&player(20.0, 0.0, 0.0), &SpendSettings::default());
        assert!(s.engineering.enabled);
        assert!(s.engineering.max_payback_days >= 60.0);
        // Mining 220 a day, so the ceilings are small -- not a copy of a big account's.
        assert!(
            s.favor.max_scrap_per_crit_point < 1_000.0,
            "{}",
            s.favor.max_scrap_per_crit_point
        );
    }

    #[test]
    fn the_ceilings_scale_with_income_rather_than_being_constants() {
        let (small, _) = spend_settings(&player(20.0, 0.0, 0.0), &SpendSettings::default());
        let (large, _) = spend_settings(&player(300.0, 0.0, 0.0), &SpendSettings::default());
        assert!(large.favor.max_scrap_per_crit_point > small.favor.max_scrap_per_crit_point * 50.0);
        assert!(large.min_scrap_reserve > small.min_scrap_reserve);
    }

    #[test]
    fn every_suggestion_carries_a_reason() {
        let (_, notes) = spend_settings(&player(100.0, 500.0, 5_000.0), &SpendSettings::default());
        assert!(notes.len() >= 5);
        for note in &notes {
            assert!(!note.field.is_empty() && !note.why.is_empty(), "{note:?}");
            assert!(note.why.len() > 30, "a reason should explain: {note:?}");
        }
    }

    #[test]
    fn what_it_has_no_opinion_on_is_left_alone() {
        let current = SpendSettings {
            enabled: true,
            delay_secs: 42,
            max_per_cycle: 7,
            damage: crate::config::DamageGoal {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let (s, _) = spend_settings(&player(20.0, 0.0, 0.0), &current);
        assert!(s.enabled);
        assert_eq!(s.delay_secs, 42);
        assert_eq!(s.max_per_cycle, 7);
        assert!(s.damage.enabled, "damage needs the battle board, not this");
    }

    #[test]
    fn rounding_produces_numbers_a_person_would_type() {
        assert_eq!(round_nicely(91_164.5), 91_000.0);
        assert_eq!(round_nicely(220.5), 220.0);
        assert_eq!(round_nicely(0.0), 0.0);
        assert_eq!(round_nicely(f64::NAN), 0.0);
    }
}
