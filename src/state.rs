//! State the bot writes and the web UI reads.
//!
//! The two live in different threads and share nothing but this: a snapshot of the
//! last cycle, a ring buffer of log lines, and a few control flags. Neither side
//! blocks the other for longer than a field copy.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::api::Player;

/// Seconds since the epoch, which is what the browser wants anyway.
pub fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What one action did, flattened for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct ActionResult {
    pub action: String,
    pub count: u32,
    pub skipped: Option<String>,
    pub failed: Option<String>,
}

/// The parts of a player the UI shows. A copy rather than the whole model, so the
/// API's shape and the UI's contract can move independently.
#[derive(Debug, Clone, Serialize)]
pub struct PlayerSnapshot {
    pub level: f64,
    pub attacks: f64,
    pub claims: f64,
    pub scrap: f64,
    pub stash_capacity: f64,
    pub stash_full: bool,
    pub wallet_scrap: f64,
    pub staked: f64,
    pub flux: f64,
    pub damage: f64,
    pub defense: f64,
    pub engineering: f64,
    pub dodge: f64,
}

impl From<&Player> for PlayerSnapshot {
    fn from(p: &Player) -> Self {
        Self {
            level: p.level,
            attacks: p.attacks,
            claims: p.claims,
            scrap: p.scrap,
            stash_capacity: p.stash_capacity(),
            stash_full: p.stash_is_full(),
            wallet_scrap: p.hive_engine_scrap,
            staked: p.hive_engine_stake,
            flux: p.flux,
            damage: p.stats.damage,
            defense: p.stats.defense,
            engineering: p.stats.engineering,
            dodge: p.stats.dodge,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AccountStatus {
    /// When this snapshot was taken. The UI shows it, because a paused bot's
    /// numbers are last cycle's numbers and pretending otherwise would mislead.
    pub at: u64,
    pub player: Option<PlayerSnapshot>,
    pub actions: Vec<ActionResult>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StatusBoard {
    pub cycle: u64,
    pub cycle_started: u64,
    pub cycle_finished: u64,
    pub running: bool,
    pub dry_run: bool,
    pub blacklist_size: usize,
    pub accounts: BTreeMap<String, AccountStatus>,
}

/// Flags the UI sets and the bot reads at safe points.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Control {
    /// Finish the cycle in flight, then stop starting new ones.
    pub paused: bool,
    /// Start a cycle now rather than waiting out the interval.
    pub run_now: bool,
    /// The config file changed; re-read it before the next cycle.
    pub reload: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub at: u64,
    pub text: String,
}

/// A bounded tail of the log. Bounded on purpose: an unbounded buffer in a process
/// meant to run for months is a leak with a nice name.
#[derive(Debug, Default)]
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    /// Monotonic, so a poller can ask for "everything after n" without timestamps.
    next_id: u64,
    capacity: usize,
}

impl LogBuffer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity),
            next_id: 0,
            capacity,
        }
    }

    pub fn push(&mut self, text: String) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.next_id += 1;
        self.lines.push_back(LogLine {
            at: epoch_secs(),
            text,
        });
    }

    /// The last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        self.lines.iter().rev().take(n).rev().cloned().collect()
    }
}

/// Everything shared between the bot thread and the web thread.
pub struct Shared {
    pub status: Mutex<StatusBoard>,
    pub control: Mutex<Control>,
    pub log: Mutex<LogBuffer>,
}

impl Shared {
    pub fn new(log_capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(StatusBoard::default()),
            control: Mutex::new(Control::default()),
            log: Mutex::new(LogBuffer::with_capacity(log_capacity)),
        })
    }

    /// A poisoned lock means another thread panicked while holding it. The data here
    /// is a status snapshot, not an invariant -- carrying on with it is strictly
    /// better than taking the bot down over a display value.
    pub fn status(&self) -> std::sync::MutexGuard<'_, StatusBoard> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn log(&self) -> std::sync::MutexGuard<'_, LogBuffer> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_buffer_stays_bounded_and_keeps_the_newest() {
        let mut buffer = LogBuffer::with_capacity(3);
        for i in 0..10 {
            buffer.push(format!("line {i}"));
        }
        let tail = buffer.tail(10);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].text, "line 7");
        assert_eq!(tail[2].text, "line 9");
    }

    #[test]
    fn a_shorter_tail_still_ends_at_the_newest_line() {
        let mut buffer = LogBuffer::with_capacity(10);
        for i in 0..5 {
            buffer.push(format!("line {i}"));
        }
        let tail = buffer.tail(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[1].text, "line 4");
    }
}
