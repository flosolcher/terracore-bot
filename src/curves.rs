//! The game's own stat curves, and what the next point of each one costs.
//!
//! Every formula here is transcribed from the game client's bundle rather than
//! guessed, including two details that look like typos and are not: `get_luck`
//! compares with `>` where dodge and crit use `>=`, and the first crit band divides
//! by `2^0 = 1`, so it does nothing at all.
//!
//! All of it is pure arithmetic, so the spending policy can be tested without a
//! network, an account, or a single broadcast.

/// Diminishing-return bands. Once a raw value passes a band, everything above it is
/// divided by `2^i` -- so each band roughly doubles the price of the next point.
fn banded(raw: f64, bands: &[f64], inclusive: bool) -> f64 {
    let mut n = raw;
    for (i, &band) in bands.iter().enumerate() {
        let past = if inclusive { n >= band } else { n > band };
        if past {
            n = band + (n - band) / 2f64.powi(i as i32);
        }
    }
    n.min(100.0)
}

const CRIT_BANDS: [f64; 14] = [
    3.0, 5.0, 7.0, 8.0, 10.0, 12.0, 14.0, 20.0, 25.0, 30.0, 40.0, 50.0, 60.0, 75.0,
];
const DODGE_BANDS: [f64; 28] = [
    3.0, 5.0, 7.0, 10.0, 12.0, 15.0, 17.0, 20.0, 25.0, 30.0, 35.0, 40.0, 42.0, 44.0, 46.0, 48.0,
    50.0, 52.0, 54.0, 56.0, 58.0, 60.0, 65.0, 70.0, 75.0, 80.0, 85.0, 90.0,
];
const LUCK_BANDS: [f64; 39] = [
    1.0, 2.0, 3.0, 5.0, 7.0, 8.0, 10.0, 12.0, 14.0, 16.0, 18.0, 20.0, 21.0, 22.0, 23.0, 25.0, 26.0,
    27.0, 28.0, 29.0, 30.0, 33.0, 35.0, 38.0, 40.0, 45.0, 48.0, 50.0, 52.0, 54.0, 56.0, 58.0, 70.0,
    75.0, 80.0, 82.0, 85.0, 88.0, 90.0,
];

/// Critical-hit percentage from favor. Favor is bought 1:1 with burned SCRAP.
pub fn crit_from_favor(favor: f64) -> f64 {
    banded(0.025 * favor.max(0.0), &CRIT_BANDS, true)
}

/// Dodge percentage from staked SCRAP.
pub fn dodge_from_stake(stake: f64) -> f64 {
    banded(0.025 * stake.max(0.0), &DODGE_BANDS, true)
}

/// Luck percentage from staked SCRAP. `>` rather than `>=`, as the client has it.
pub fn luck_from_stake(stake: f64) -> f64 {
    banded(0.025 * stake.max(0.0), &LUCK_BANDS, false)
}

/// Engineering above this is worth half as much per point.
pub const ENGINEERING_SOFTCAP: f64 = 333.0;

/// SCRAP mined per day at a given effective engineering.
pub fn minerate_per_day(engineering: f64) -> f64 {
    let e = engineering.max(0.0);
    let effective = if e > ENGINEERING_SOFTCAP {
        ENGINEERING_SOFTCAP + (e - ENGINEERING_SOFTCAP) * 0.5
    } else {
        e
    };
    // The client's per-second rate is (i+1)^2 / (48*3600); a day is half that squared
    // term because 86400/172800 = 0.5.
    (effective + 1.0).powi(2) / 2.0
}

/// What the game charges to raise a base stat by one point.
pub fn stat_cost(stat: crate::config::Stat, current: f64) -> f64 {
    match stat {
        crate::config::Stat::Engineering => current * current,
        crate::config::Stat::Damage | crate::config::Stat::Defense => {
            (current / 10.0) * (current / 10.0)
        }
    }
}

/// Days for one more point of engineering to mine back what it cost.
///
/// Below the softcap this lands almost exactly on the current level: engineering 30
/// repays in about 30 days, engineering 100 in about 100. Above it, the halved rate
/// makes the number explode, which is the natural place to stop.
pub fn engineering_payback_days(engineering: f64) -> f64 {
    let gain = minerate_per_day(engineering + 1.0) - minerate_per_day(engineering);
    if gain <= 0.0 {
        return f64::INFINITY;
    }
    stat_cost(crate::config::Stat::Engineering, engineering) / gain
}

/// Smallest input for which `f` reaches `target`, or `None` past the curve's ceiling.
///
/// Bisection rather than an algebraic inverse: the band structure is piecewise and
/// inverting it by hand is exactly the sort of thing that is wrong in a way no test
/// would notice.
fn invert(f: impl Fn(f64) -> f64, target: f64, limit: f64) -> Option<f64> {
    if f(limit) < target {
        return None;
    }
    let (mut lo, mut hi) = (0.0f64, limit);
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        if f(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(hi)
}

/// Favor needed to reach a crit percentage.
pub fn favor_for_crit(target: f64) -> Option<f64> {
    invert(crit_from_favor, target, 1e15)
}

/// Stake needed to reach a dodge percentage.
pub fn stake_for_dodge(target: f64) -> Option<f64> {
    invert(dodge_from_stake, target, 1e15)
}

/// How many percentage points to look ahead when measuring a marginal price.
///
/// Small on purpose. Measuring a whole point ahead sees the *next* cliff from inside
/// the current band and stops short of the edge -- which made favor buying halt at
/// 11.05% when 12% was still being sold at the same price.
const MARGIN_STEP: f64 = 0.001;

/// SCRAP per additional percent of crit, measured locally.
///
/// This is the number that decides whether more favor is worth buying: it is flat
/// within a band and doubles at every band edge, so a ceiling on it stops spending
/// exactly at the cliff rather than at a percentage guessed in advance.
pub fn scrap_per_crit_point(favor: f64) -> f64 {
    let here = crit_from_favor(favor);
    match favor_for_crit(here + MARGIN_STEP) {
        Some(needed) => (needed - favor).max(0.0) / MARGIN_STEP,
        None => f64::INFINITY,
    }
}

/// How much more favor is worth buying before the next percent stops being worth
/// `max_scrap_per_point`.
///
/// The marginal price only ever rises, so this is the distance to the band edge
/// where it crosses the ceiling -- which is what lets a config say "keep going while
/// a point costs less than X" instead of naming a percentage that will be wrong in a
/// month.
pub fn favor_affordable(favor: f64, max_scrap_per_point: f64) -> f64 {
    if scrap_per_crit_point(favor) > max_scrap_per_point {
        return 0.0;
    }
    let (mut lo, mut hi) = (0.0f64, 1e12f64);
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        if scrap_per_crit_point(favor + mid) <= max_scrap_per_point {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// SCRAP per additional percent of dodge, measured locally. See
/// [`scrap_per_crit_point`] for why this is not a whole-point lookahead.
pub fn scrap_per_dodge_point(stake: f64) -> f64 {
    let here = dodge_from_stake(stake);
    match stake_for_dodge(here + MARGIN_STEP) {
        Some(needed) => (needed - stake).max(0.0) / MARGIN_STEP,
        None => f64::INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Stat;

    /// The anchor for this whole module: a real account showing 44,000 favor and
    /// 10.933% crit in the game's own UI. If the transcription of the band maths
    /// were wrong, this is the test that would say so.
    #[test]
    fn crit_matches_the_games_own_display() {
        assert!(
            (crit_from_favor(44_000.0) - 10.933).abs() < 0.001,
            "got {}",
            crit_from_favor(44_000.0)
        );
    }

    #[test]
    fn the_crit_bands_are_where_the_client_puts_them() {
        // Linear at 0.025/point until the first band that actually divides.
        assert!((crit_from_favor(120.0) - 3.0).abs() < 1e-9);
        assert!((crit_from_favor(200.0) - 5.0).abs() < 1e-9);
        assert!((crit_from_favor(360.0) - 7.0).abs() < 1e-9);
        assert!((crit_from_favor(680.0) - 8.0).abs() < 1e-9);
        assert_eq!(crit_from_favor(0.0), 0.0);
        assert_eq!(crit_from_favor(-5.0), 0.0);
    }

    #[test]
    fn crit_is_flat_within_a_band_and_doubles_at_the_edge() {
        // From 10.933% the next point costs 40,960 all the way to 12%...
        let here = scrap_per_crit_point(44_000.0);
        assert!((here - 40_960.0).abs() < 1.0, "got {here}");
        // ...and past 12% it costs 32 times as much.
        let past = scrap_per_crit_point(90_000.0);
        assert!((past - 1_310_720.0).abs() < 1.0, "got {past}");
        assert!(past / here > 30.0);
    }

    #[test]
    fn dodge_and_luck_track_stake_and_luck_uses_the_stricter_test() {
        assert!((dodge_from_stake(1_000.0) - 9.0).abs() < 0.01);
        assert!((dodge_from_stake(10_000.0) - 12.09).abs() < 0.01);
        assert!((luck_from_stake(1_000.0) - 5.078).abs() < 0.01);
        assert!((luck_from_stake(800.0) - 5.0).abs() < 1e-9);
        // Exactly on a shared band, `>` and `>=` part company: 0.025 * 120 = 3.0 is a
        // band in both tables, dodge folds nothing and stops at 3, luck has already
        // folded twice on its way past 1 and 2 and lands at 2.5. Getting this
        // backwards would misprice every stake decision, so it is pinned.
        assert_eq!(dodge_from_stake(120.0), 3.0);
        assert_eq!(luck_from_stake(120.0), 2.5);
    }

    #[test]
    fn mining_income_matches_the_clients_rate() {
        // (0+1)^2 / 2 for a brand new account.
        assert!((minerate_per_day(0.0) - 0.5).abs() < 1e-9);
        // Softcap: above 333 each point counts half.
        assert!((minerate_per_day(333.0) - 55_778.0).abs() < 1.0);
        assert!((minerate_per_day(519.0) - 91_164.5).abs() < 1.0);
        // Past the cap the curve is strictly flatter than below it.
        let below = minerate_per_day(101.0) - minerate_per_day(100.0);
        let above = minerate_per_day(501.0) - minerate_per_day(500.0);
        assert!(
            above < below * 3.0,
            "the softcap must bite: {below} vs {above}"
        );
    }

    #[test]
    fn engineering_payback_is_roughly_the_current_level_in_days() {
        // Just under the level, converging upward as the level rises: 0.930 at 20,
        // 0.985 at 100, 0.993 at 200.
        let mut previous = 0.0;
        for level in [20.0, 50.0, 100.0, 200.0] {
            let ratio = engineering_payback_days(level) / level;
            assert!(
                (0.9..1.0).contains(&ratio),
                "at {level} the ratio was {ratio}"
            );
            assert!(ratio > previous, "the ratio must climb toward 1");
            previous = ratio;
        }
        // And the softcap makes it explode, which is the signal to stop.
        assert!(engineering_payback_days(519.0) > 1_000.0);
    }

    #[test]
    fn stat_costs_are_the_games_own() {
        assert_eq!(stat_cost(Stat::Engineering, 519.0), 269_361.0);
        assert_eq!(stat_cost(Stat::Damage, 3290.0), 108_241.0);
        assert_eq!(stat_cost(Stat::Defense, 100.0), 100.0);
    }

    #[test]
    fn inverting_a_curve_returns_the_point_it_was_asked_for() {
        for target in [1.0, 5.0, 8.0, 11.0] {
            let favor = favor_for_crit(target).expect("reachable");
            assert!(
                (crit_from_favor(favor) - target).abs() < 1e-6,
                "at {target}"
            );
        }
        // Nothing buys 100% crit, and saying so beats returning a huge number.
        assert!(favor_for_crit(100.0).is_none());
    }

    #[test]
    fn favor_buying_stops_at_the_cliff_not_at_a_guessed_percentage() {
        // From 44,000 favor a point costs 40,960 up to 12% crit, then 1,310,720.
        // A ceiling of 100,000 should therefore buy exactly up to the 12% edge.
        let room = favor_affordable(44_000.0, 100_000.0);
        let landing = crit_from_favor(44_000.0 + room);
        assert!((landing - 12.0).abs() < 0.01, "landed on {landing}%");
        // And the spend needed to get there matches the hand calculation.
        assert!((room - 43_720.0).abs() < 100.0, "room was {room}");

        // The bug this guards: measuring a whole point ahead saw the 12% cliff from
        // inside the flat band and stopped at 11.05%, leaving a point of crit on the
        // table that was still selling at 40,960.
        assert!(landing > 11.9, "stopped short of the cliff at {landing}%");

        // A ceiling below the current marginal price buys nothing at all.
        assert_eq!(favor_affordable(44_000.0, 1_000.0), 0.0);
        // A ceiling above the next cliff carries on past it.
        assert!(crit_from_favor(44_000.0 + favor_affordable(44_000.0, 2_000_000.0)) > 12.5);
    }

    #[test]
    fn a_marginal_cost_never_comes_back_negative() {
        for favor in [0.0, 1.0, 119.0, 44_000.0, 5_000_000.0] {
            assert!(scrap_per_crit_point(favor) >= 0.0);
        }
        for stake in [0.0, 1_000.0, 130_000.0, 9_000_000.0] {
            assert!(scrap_per_dodge_point(stake) >= 0.0);
        }
    }
}
