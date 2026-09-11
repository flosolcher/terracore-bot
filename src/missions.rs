//! Whether a mission on today's board can be started.
//!
//! Ported from the game client's own predicate rather than inferred, because
//! starting a mission the server will refuse burns the SCRAP either way. The client
//! checks five things, and so does this: the day's board is current, the mission is
//! not already running, your level clears the tier, the relevant stat clears the
//! tier, and from tier 3 up the matching gear slot is filled.

use std::collections::HashSet;

use crate::api::{BoardSlot, Items, Player, Quest};

/// Identifies one mission for the day: the game allows a given type and tier once
/// per board, so this is the natural key for "already dealt with".
pub type MissionKey = (String, String, u8);

pub fn key_for(date: &str, slot: &BoardSlot) -> MissionKey {
    (
        date.to_string(),
        slot.quest_type.clone(),
        slot.tier.round() as u8,
    )
}

/// Minimum player level per tier.
const LEVEL_FOR_TIER: [(u8, f64); 5] = [(1, 1.0), (2, 10.0), (3, 25.0), (4, 50.0), (5, 100.0)];
/// Minimum stat per tier for the flat stats (damage, engineering, defense).
const STAT_FOR_TIER: [(u8, f64); 5] = [(1, 10.0), (2, 50.0), (3, 100.0), (4, 200.0), (5, 500.0)];
/// Minimum stat per tier for the percentage stats (luck, dodge), which are smaller
/// numbers and so have their own much lower table.
const PERCENT_STAT_FOR_TIER: [(u8, f64); 5] = [(1, 2.0), (2, 5.0), (3, 12.0), (4, 20.0), (5, 40.0)];

/// Which stat a mission type is judged on.
pub fn stat_for(quest_type: &str) -> &'static str {
    match quest_type {
        "combat" => "damage",
        "salvage" => "engineering",
        "stealth" => "dodge",
        "fortune" => "luck",
        "defense" => "defense",
        _ => "damage",
    }
}

/// Which gear slot a mission type requires from tier 3 upwards.
pub fn slot_for(quest_type: &str) -> &'static str {
    match quest_type {
        "combat" => "weapon",
        "salvage" => "special",
        "stealth" => "armor",
        "fortune" => "avatar",
        "defense" => "ship",
        _ => "avatar",
    }
}

/// Luck and dodge are summed across every equipped item; everything else adds the
/// matching slot's contribution to the player's effective stat.
fn is_percentage_stat(stat: &str) -> bool {
    stat == "luck" || stat == "dodge"
}

fn table(t: &[(u8, f64); 5], tier: f64) -> Option<f64> {
    let tier = tier.round();
    if !(1.0..=5.0).contains(&tier) {
        return None;
    }
    t.iter()
        .find(|(n, _)| f64::from(*n) == tier)
        .map(|(_, v)| *v)
}

/// The stat value this mission is judged against, exactly as the client computes it.
///
/// Note the asymmetry, which is the client's and not a mistake here: percentage
/// stats come only from item attributes, while the others add the matching slot's
/// attribute on top of the player's effective stat.
pub fn qualifying_stat(player: &Player, items: &Items, quest_type: &str) -> f64 {
    let stat = stat_for(quest_type);
    if is_percentage_stat(stat) {
        return items
            .all()
            .iter()
            .map(|item| attribute(&item.attributes, stat))
            .sum();
    }
    let base = attribute(&player.stats, stat);
    base + attribute(&items.slot(slot_for(quest_type)).attributes, stat)
}

fn attribute(stats: &crate::api::Stats, name: &str) -> f64 {
    match name {
        "damage" => stats.damage,
        "defense" => stats.defense,
        "engineering" => stats.engineering,
        "dodge" => stats.dodge,
        "luck" => stats.luck,
        "crit" => stats.crit,
        _ => 0.0,
    }
}

/// Why a mission cannot be started.
#[derive(Debug, Clone, PartialEq)]
pub enum Blocked {
    /// The board is not today's, so its slots may no longer exist. Starting one
    /// would burn SCRAP for a mission that has gone.
    StaleBoard,
    AlreadyRunning,
    UnknownTier,
    LevelTooLow {
        need: u32,
    },
    StatTooLow {
        stat: &'static str,
        need: f64,
    },
    SlotEmpty {
        slot: &'static str,
    },
    CannotAfford {
        cost: f64,
    },
}

impl Blocked {
    pub fn describe(&self) -> String {
        match self {
            Blocked::StaleBoard => "the mission board is not today's; it has reset".into(),
            Blocked::AlreadyRunning => "already started today".into(),
            Blocked::UnknownTier => "unknown tier".into(),
            Blocked::LevelTooLow { need } => format!("needs level {need}"),
            Blocked::StatTooLow { stat, need } => format!("needs {stat} of {need:.0}"),
            Blocked::SlotEmpty { slot } => format!("tier 3+ needs a {slot} equipped"),
            Blocked::CannotAfford { cost } => format!("costs {cost:.0} liquid SCRAP"),
        }
    }
}

/// Today's date in the form the board uses.
pub fn today_utc(now_ms: i64) -> String {
    // Days since the epoch, converted without pulling in a calendar crate: the board
    // only ever needs to be compared for equality with its own `date`.
    let days = now_ms.div_euclid(86_400_000);
    let (mut year, mut remaining) = (1970i64, days);
    loop {
        let len = if is_leap(year) { 366 } else { 365 };
        if remaining < len {
            break;
        }
        remaining -= len;
        year += 1;
    }
    let lengths = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0;
    while remaining >= lengths[month] {
        remaining -= lengths[month];
        month += 1;
    }
    format!("{year:04}-{:02}-{:02}", month + 1, remaining + 1)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Whether this slot can be started right now.
///
/// `spendable` is the liquid SCRAP genuinely available -- the caller subtracts any
/// reserve before asking, so this does not need to know about one.
pub fn blocked(
    slot: &BoardSlot,
    board_date: &str,
    today: &str,
    player: &Player,
    items: &Items,
    running: &[Quest],
    spendable: f64,
) -> Option<Blocked> {
    if board_date != today {
        return Some(Blocked::StaleBoard);
    }
    // The client keys "already started" on type and tier for the current board day.
    if running.iter().any(|q| {
        q.quest_type == slot.quest_type && q.tier.round() == slot.tier.round() && !q.collected
    }) {
        return Some(Blocked::AlreadyRunning);
    }

    let Some(level_needed) = table(&LEVEL_FOR_TIER, slot.tier) else {
        return Some(Blocked::UnknownTier);
    };
    if player.level < level_needed {
        return Some(Blocked::LevelTooLow {
            need: level_needed as u32,
        });
    }

    let stat = stat_for(&slot.quest_type);
    let needed = if is_percentage_stat(stat) {
        table(&PERCENT_STAT_FOR_TIER, slot.tier)
    } else {
        table(&STAT_FOR_TIER, slot.tier)
    };
    let Some(needed) = needed else {
        return Some(Blocked::UnknownTier);
    };
    if qualifying_stat(player, items, &slot.quest_type) < needed {
        return Some(Blocked::StatTooLow { stat, need: needed });
    }

    if slot.tier.round() >= 3.0 {
        let slot_name = slot_for(&slot.quest_type);
        if !items.slot(slot_name).equipped() {
            return Some(Blocked::SlotEmpty { slot: slot_name });
        }
    }

    if spendable < slot.scrap_cost {
        return Some(Blocked::CannotAfford {
            cost: slot.scrap_cost,
        });
    }
    None
}

/// Everything a mission decision depends on, gathered so the rule itself stays
/// testable without a network or a clock -- the same shape the targeting module uses.
pub struct Context<'a> {
    pub today: &'a str,
    pub player: &'a Player,
    pub items: &'a Items,
    pub running: &'a [Quest],
    pub settings: &'a crate::config::QuestSettings,
    /// Liquid SCRAP genuinely available: the caller has already taken off any
    /// reserve, so this does not need to know about one.
    pub spendable: f64,
    /// Missions this process has already started today.
    pub already_started: &'a HashSet<MissionKey>,
}

/// The board slots worth starting, best first, plus why each of the rest was not.
///
/// Everything the choice depends on lives here rather than in the action: the
/// settings filters used to sit in the broadcasting function where no test could
/// reach them, and mutation testing duly found that removing the tier range and the
/// cost cap changed nothing any test could see.
pub fn select<'a>(
    board: &'a crate::api::QuestBoard,
    ctx: &Context<'_>,
) -> (Vec<&'a BoardSlot>, Vec<String>) {
    let Context {
        today,
        player,
        items,
        running,
        settings: s,
        spendable,
        already_started,
    } = ctx;
    let spendable = *spendable;
    let mut candidates: Vec<&BoardSlot> = Vec::new();
    let mut refused: Vec<String> = Vec::new();

    for slot in &board.slots {
        let tier = slot.tier.round() as u8;
        if tier < s.min_tier || tier > s.max_tier {
            continue;
        }
        if !s.types.is_empty() && !s.types.iter().any(|t| t == &slot.quest_type) {
            continue;
        }
        if s.max_scrap_per_mission > 0.0 && slot.scrap_cost > s.max_scrap_per_mission {
            refused.push(format!(
                "t{tier} {}: costs {:.0}, over max_scrap_per_mission",
                slot.quest_type, slot.scrap_cost
            ));
            continue;
        }
        // The game queues these, so `running` can still be empty seconds after a
        // start went out. Without a memory of what this process already sent, a
        // second cycle -- a "run now" from the panel, say -- would start the same
        // mission again and burn the cost twice.
        if already_started.contains(&key_for(&board.date, slot)) {
            refused.push(format!(
                "t{tier} {}: already started by this run today",
                slot.quest_type
            ));
            continue;
        }
        match blocked(slot, &board.date, today, player, items, running, spendable) {
            None => candidates.push(slot),
            Some(reason) => refused.push(format!(
                "t{tier} {}: {}",
                slot.quest_type,
                reason.describe()
            )),
        }
    }

    let cmp = |a: &&BoardSlot, b: &&BoardSlot| match s.order {
        crate::config::MissionOrder::HighestTier => b
            .tier
            .partial_cmp(&a.tier)
            .unwrap_or(std::cmp::Ordering::Equal),
        crate::config::MissionOrder::Cheapest => a
            .scrap_cost
            .partial_cmp(&b.scrap_cost)
            .unwrap_or(std::cmp::Ordering::Equal),
        crate::config::MissionOrder::BestValue => {
            let value = |m: &BoardSlot| {
                if m.scrap_cost > 0.0 {
                    m.base_rolls / m.scrap_cost
                } else {
                    f64::INFINITY
                }
            };
            value(b)
                .partial_cmp(&value(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        }
    };
    candidates.sort_by(cmp);
    (candidates, refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Item, Stats};

    fn player(level: f64, damage: f64) -> Player {
        Player {
            level,
            stats: Stats {
                damage,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn slot(quest_type: &str, tier: f64, cost: f64) -> BoardSlot {
        BoardSlot {
            quest_type: quest_type.into(),
            tier,
            scrap_cost: cost,
            ..Default::default()
        }
    }

    fn equipped(stat_value: f64) -> Item {
        Item {
            item_number: Some(1),
            attributes: Stats {
                damage: stat_value,
                dodge: stat_value,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    const TODAY: &str = "2026-09-11";

    #[test]
    fn a_tier_one_combat_mission_needs_level_1_and_damage_10() {
        let p = player(1.0, 10.0);
        let items = Items::default();
        assert_eq!(
            blocked(
                &slot("combat", 1.0, 100.0),
                TODAY,
                TODAY,
                &p,
                &items,
                &[],
                100.0
            ),
            None
        );
        // One short on damage and it is refused, naming the stat.
        let weak = player(1.0, 9.0);
        assert_eq!(
            blocked(
                &slot("combat", 1.0, 100.0),
                TODAY,
                TODAY,
                &weak,
                &items,
                &[],
                100.0
            ),
            Some(Blocked::StatTooLow {
                stat: "damage",
                need: 10.0
            })
        );
    }

    #[test]
    fn the_level_gate_matches_the_clients_table() {
        let items = Items::default();
        for (tier, need) in [(1.0, 1), (2.0, 10), (3.0, 25), (4.0, 50), (5.0, 100)] {
            let p = player(f64::from(need) - 1.0, 10_000.0);
            let got = blocked(
                &slot("combat", tier, 1.0),
                TODAY,
                TODAY,
                &p,
                &items,
                &[],
                1e9,
            );
            assert_eq!(got, Some(Blocked::LevelTooLow { need }), "tier {tier}");
        }
    }

    #[test]
    fn tier_three_and_up_need_the_matching_slot_filled() {
        let p = player(100.0, 10_000.0);
        let empty = Items::default();
        assert_eq!(
            blocked(
                &slot("combat", 3.0, 1.0),
                TODAY,
                TODAY,
                &p,
                &empty,
                &[],
                1e9
            ),
            Some(Blocked::SlotEmpty { slot: "weapon" })
        );
        // With a weapon on, it passes.
        let armed = Items {
            weapon: equipped(0.0),
            ..Default::default()
        };
        assert_eq!(
            blocked(
                &slot("combat", 3.0, 1.0),
                TODAY,
                TODAY,
                &p,
                &armed,
                &[],
                1e9
            ),
            None
        );
        // Tier 2 never asks.
        assert_eq!(
            blocked(
                &slot("combat", 2.0, 1.0),
                TODAY,
                TODAY,
                &p,
                &empty,
                &[],
                1e9
            ),
            None
        );
    }

    #[test]
    fn percentage_missions_count_item_attributes_only() {
        // A stealth mission is judged on dodge, and dodge for this purpose comes
        // from equipped gear -- not from the dodge the account gets for staking.
        let mut p = player(100.0, 0.0);
        p.stats.dodge = 90.0; // staked dodge, which must not count here
        let bare = Items::default();
        assert_eq!(
            blocked(
                &slot("stealth", 1.0, 1.0),
                TODAY,
                TODAY,
                &p,
                &bare,
                &[],
                1e9
            ),
            Some(Blocked::StatTooLow {
                stat: "dodge",
                need: 2.0
            })
        );

        // Two pieces contributing 1 dodge each clear the tier-1 requirement of 2.
        let geared = Items {
            weapon: equipped(1.0),
            armor: equipped(1.0),
            ..Default::default()
        };
        assert_eq!(qualifying_stat(&p, &geared, "stealth"), 2.0);
        assert_eq!(
            blocked(
                &slot("stealth", 1.0, 1.0),
                TODAY,
                TODAY,
                &p,
                &geared,
                &[],
                1e9
            ),
            None
        );
    }

    #[test]
    fn flat_missions_add_the_matching_slot_on_top_of_the_player_stat() {
        let p = player(100.0, 8.0);
        let armed = Items {
            weapon: equipped(5.0),
            ..Default::default()
        };
        assert_eq!(qualifying_stat(&p, &armed, "combat"), 13.0);
        assert_eq!(
            blocked(
                &slot("combat", 1.0, 1.0),
                TODAY,
                TODAY,
                &p,
                &armed,
                &[],
                1e9
            ),
            None
        );
    }

    #[test]
    fn a_stale_board_is_refused_before_anything_else() {
        let p = player(100.0, 10_000.0);
        // Everything else about this would pass.
        assert_eq!(
            blocked(
                &slot("combat", 1.0, 1.0),
                "2026-09-10",
                TODAY,
                &p,
                &Items::default(),
                &[],
                1e9
            ),
            Some(Blocked::StaleBoard)
        );
    }

    #[test]
    fn a_mission_already_running_is_not_started_twice() {
        let p = player(100.0, 10_000.0);
        let running = vec![Quest {
            quest_type: "combat".into(),
            tier: 1.0,
            ..Default::default()
        }];
        assert_eq!(
            blocked(
                &slot("combat", 1.0, 1.0),
                TODAY,
                TODAY,
                &p,
                &Items::default(),
                &running,
                1e9
            ),
            Some(Blocked::AlreadyRunning)
        );
        // A collected one no longer blocks.
        let done = vec![Quest {
            quest_type: "combat".into(),
            tier: 1.0,
            collected: true,
            ..Default::default()
        }];
        assert_eq!(
            blocked(
                &slot("combat", 1.0, 1.0),
                TODAY,
                TODAY,
                &p,
                &Items::default(),
                &done,
                1e9
            ),
            None
        );
    }

    #[test]
    fn affordability_is_checked_against_what_the_caller_says_is_spendable() {
        let p = player(100.0, 10_000.0);
        let items = Items::default();
        assert_eq!(
            blocked(
                &slot("combat", 1.0, 500.0),
                TODAY,
                TODAY,
                &p,
                &items,
                &[],
                499.0
            ),
            Some(Blocked::CannotAfford { cost: 500.0 })
        );
        assert_eq!(
            blocked(
                &slot("combat", 1.0, 500.0),
                TODAY,
                TODAY,
                &p,
                &items,
                &[],
                500.0
            ),
            None
        );
    }

    #[test]
    fn the_date_is_rendered_the_way_the_board_writes_it() {
        assert_eq!(today_utc(0), "1970-01-01");
        // 2026-09-11T00:00:00Z
        assert_eq!(today_utc(1_789_084_800_000), "2026-09-11");
        // A leap day, which a naive month table gets wrong.
        assert_eq!(today_utc(1_709_164_800_000), "2024-02-29");
        // And the day after it.
        assert_eq!(today_utc(1_709_251_200_000), "2024-03-01");
    }
}

#[cfg(test)]
mod selection_tests {
    //! The filters and the ordering, which used to live in the broadcasting
    //! function where no test could reach them.
    use super::*;
    use crate::api::QuestBoard;
    use crate::config::{MissionOrder, QuestSettings, Settings};

    const TODAY: &str = "2026-09-11";

    fn board(slots: &[(&str, f64, f64, f64)]) -> QuestBoard {
        QuestBoard {
            date: TODAY.into(),
            slots: slots
                .iter()
                .map(|(kind, tier, cost, rolls)| BoardSlot {
                    quest_type: (*kind).into(),
                    tier: *tier,
                    scrap_cost: *cost,
                    base_rolls: *rolls,
                    name: format!("t{tier} {kind}"),
                    ..Default::default()
                })
                .collect(),
        }
    }

    /// Strong enough for anything on the board, so only the settings filters bite.
    fn strong() -> Player {
        Player {
            level: 100.0,
            stats: crate::api::Stats {
                damage: 100_000.0,
                engineering: 100_000.0,
                defense: 100_000.0,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn geared() -> Items {
        let item = crate::api::Item {
            item_number: Some(1),
            ..Default::default()
        };
        Items {
            avatar: item.clone(),
            weapon: item.clone(),
            armor: item.clone(),
            ship: item.clone(),
            special: item,
        }
    }

    fn settings() -> QuestSettings {
        let mut s = Settings::default().quest;
        s.start = true;
        s
    }

    fn ctx<'a>(
        s: &'a QuestSettings,
        player: &'a Player,
        items: &'a Items,
        already: &'a std::collections::HashSet<MissionKey>,
    ) -> Context<'a> {
        Context {
            today: TODAY,
            player,
            items,
            running: &[],
            settings: s,
            spendable: 1e9,
            already_started: already,
        }
    }

    fn names(picked: &[&BoardSlot]) -> Vec<String> {
        picked.iter().map(|s| s.name.clone()).collect()
    }

    #[test]
    fn the_tier_range_is_respected() {
        let b = board(&[
            ("combat", 1.0, 10.0, 2.0),
            ("combat", 3.0, 30.0, 4.0),
            ("combat", 5.0, 50.0, 8.0),
        ]);
        let mut s = settings();
        s.min_tier = 2;
        s.max_tier = 4;
        let (picked, _) = select(&b, &ctx(&s, &strong(), &geared(), &HashSet::new()));
        assert_eq!(names(&picked), ["t3 combat"], "only tier 3 is inside 2..=4");
    }

    #[test]
    fn the_cost_cap_is_respected_and_says_so() {
        let b = board(&[("combat", 1.0, 100.0, 2.0), ("combat", 1.0, 5000.0, 2.0)]);
        let mut s = settings();
        s.max_scrap_per_mission = 1000.0;
        let (picked, refused) = select(&b, &ctx(&s, &strong(), &geared(), &HashSet::new()));
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].scrap_cost, 100.0);
        assert!(
            refused.iter().any(|r| r.contains("max_scrap_per_mission")),
            "{refused:?}"
        );
    }

    #[test]
    fn the_type_filter_is_respected() {
        let b = board(&[("combat", 1.0, 10.0, 2.0), ("salvage", 1.0, 10.0, 2.0)]);
        let mut s = settings();
        s.types = vec!["salvage".into()];
        let (picked, _) = select(&b, &ctx(&s, &strong(), &geared(), &HashSet::new()));
        assert_eq!(names(&picked), ["t1 salvage"]);
    }

    #[test]
    fn each_ordering_picks_a_different_mission_first() {
        // Cheap-but-poor, dear-but-rich, and the best rolls per SCRAP.
        let b = board(&[
            ("combat", 1.0, 100.0, 2.0),   // 0.0200 rolls/scrap
            ("combat", 5.0, 5000.0, 10.0), // 0.0020 rolls/scrap, highest tier
            ("combat", 2.0, 200.0, 9.0),   // 0.0450 rolls/scrap, best value
        ]);
        let mut s = settings();

        s.order = MissionOrder::Cheapest;
        let (picked, _) = select(&b, &ctx(&s, &strong(), &geared(), &HashSet::new()));
        assert_eq!(picked[0].scrap_cost, 100.0, "cheapest first");

        s.order = MissionOrder::HighestTier;
        let (picked, _) = select(&b, &ctx(&s, &strong(), &geared(), &HashSet::new()));
        assert_eq!(picked[0].tier, 5.0, "highest tier first");

        s.order = MissionOrder::BestValue;
        let (picked, _) = select(&b, &ctx(&s, &strong(), &geared(), &HashSet::new()));
        assert_eq!(picked[0].base_rolls, 9.0, "most rolls per SCRAP first");
    }

    #[test]
    fn a_mission_this_run_already_started_is_not_paid_for_twice() {
        // The game records a start through a queue, so for a minute afterwards its
        // own answer still says nothing is running. Without this memory a second
        // cycle -- a "run now" from the panel -- pays the cost again.
        let b = board(&[("combat", 1.0, 641.0, 2.0)]);
        let s = settings();

        let (picked, _) = select(&b, &ctx(&s, &strong(), &geared(), &HashSet::new()));
        assert_eq!(picked.len(), 1, "it should be startable to begin with");

        let mut started = HashSet::new();
        started.insert(key_for(TODAY, picked[0]));
        let (picked, refused) = select(&b, &ctx(&s, &strong(), &geared(), &started));
        assert!(picked.is_empty(), "{picked:?}");
        assert!(refused[0].contains("already started"), "{refused:?}");
    }

    #[test]
    fn the_memory_is_keyed_to_the_day_so_it_clears_at_the_rollover() {
        let b = board(&[("combat", 1.0, 641.0, 2.0)]);
        let s = settings();
        let mut started = HashSet::new();
        started.insert(("2026-09-10".to_string(), "combat".to_string(), 1u8));

        // Yesterday's record must not block today's board.
        let (picked, _) = select(&b, &ctx(&s, &strong(), &geared(), &started));
        assert_eq!(picked.len(), 1, "a stale day must not block a fresh board");
    }

    #[test]
    fn the_started_memory_does_not_grow_without_bound() {
        // The action prunes to the current day before use. This checks the property
        // that makes that safe: a key is only ever relevant to its own date, so
        // dropping the others cannot change any decision.
        let mut set: std::collections::HashSet<MissionKey> = std::collections::HashSet::new();
        for day in 1..=28 {
            set.insert((format!("2026-08-{day:02}"), "combat".into(), 1));
        }
        set.insert((TODAY.to_string(), "combat".into(), 1));
        assert_eq!(set.len(), 29);

        set.retain(|(date, _, _)| date == TODAY);
        assert_eq!(set.len(), 1, "only today's entry survives");

        // And it is still the one that matters.
        let b = board(&[("combat", 1.0, 641.0, 2.0)]);
        let (picked, _) = select(&b, &ctx(&settings(), &strong(), &geared(), &set));
        assert!(picked.is_empty(), "today's record must still block");
    }

    #[test]
    fn a_filtered_out_mission_is_not_reported_as_refused() {
        // Out of the tier range is a choice, not a refusal -- listing it would bury
        // the reasons that actually need acting on.
        let b = board(&[("combat", 5.0, 10.0, 2.0)]);
        let mut s = settings();
        s.max_tier = 2;
        let (picked, refused) = select(&b, &ctx(&s, &strong(), &geared(), &HashSet::new()));
        assert!(picked.is_empty());
        assert!(refused.is_empty(), "{refused:?}");
    }

    #[test]
    fn the_games_own_gates_still_apply_inside_the_filters() {
        let b = board(&[("combat", 4.0, 10.0, 2.0)]);
        let weak = Player {
            level: 10.0,
            ..strong()
        };
        let (picked, refused) = select(&b, &ctx(&settings(), &weak, &geared(), &HashSet::new()));
        assert!(picked.is_empty());
        assert!(refused[0].contains("level 50"), "{refused:?}");
    }
}
