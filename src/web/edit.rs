//! Writing settings back into the config file.
//!
//! The file stays the source of truth and stays hand-editable, so this edits it in
//! place with `toml_edit` -- comments, key order and formatting survive -- rather
//! than reserializing the whole document from a struct.
//!
//! The UI sends *effective* settings, which is what a person edits. What gets written
//! is only the keys that actually differ from `[defaults]`; a value dragged back onto
//! the default has its override deleted rather than restated. That keeps the file
//! saying what it means: an override present is an override intended.

use anyhow::{bail, Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::Settings;

/// Apply one account's settings to the config text, returning the new text.
pub fn apply(text: &str, account: &str, incoming: &Settings, enabled: bool) -> Result<String> {
    let defaults = crate::config::Config::default_settings(text)
        .context("reading [defaults] to work out which values are overrides")?;

    let incoming = to_table(incoming)?;
    let defaults = to_table(&defaults)?;

    let mut doc: DocumentMut = text.parse().context("reparsing the config")?;

    // The account must already exist. Creating accounts from the web UI would mean
    // creating one whose wallet has no key, which cannot do anything.
    if !account_exists(&doc, account) {
        bail!("no [accounts.{account}] section in the config");
    }

    // The sections are whatever `Settings` serializes to, rather than a list kept in
    // step by hand: a new group of settings would otherwise be shown by the UI,
    // accepted by the server, and silently never written.
    let sections: Vec<String> = incoming.keys().cloned().collect();

    let mut path: Vec<String> = Vec::new();
    diff_into(&mut doc, account, &mut path, &incoming, &defaults)?;

    // `enabled = true` is the default, so it is written only when it is false.
    if enabled {
        unset_account_key(&mut doc, account, "enabled");
    } else {
        set_account_key(&mut doc, account, "enabled", Value::from(false));
    }

    prune_empty_sections(&mut doc, account, &sections);
    Ok(doc.to_string())
}

fn to_table(settings: &Settings) -> Result<toml::value::Table> {
    match toml::Value::try_from(settings).context("rendering settings as TOML")? {
        toml::Value::Table(table) => Ok(table),
        _ => bail!("settings did not render as a table"),
    }
}

fn account_exists(doc: &DocumentMut, account: &str) -> bool {
    doc.get("accounts")
        .and_then(Item::as_table)
        .is_some_and(|t| t.contains_key(account))
}

fn account_table<'a>(doc: &'a mut DocumentMut, account: &str) -> Option<&'a mut Table> {
    doc.get_mut("accounts")?
        .as_table_mut()?
        .get_mut(account)?
        .as_table_mut()
}

/// Walk the incoming settings against the defaults, writing only what differs.
///
/// Recursive because the settings are not flat: a goal like `spend.engineering` is
/// its own table. Walking only the top level silently dropped every nested value,
/// which meant nothing under `[spend]` could be saved at all.
fn diff_into(
    doc: &mut DocumentMut,
    account: &str,
    path: &mut Vec<String>,
    incoming: &toml::value::Table,
    defaults: &toml::value::Table,
) -> Result<()> {
    for (key, value) in incoming {
        let default_value = defaults.get(key);
        match (value, default_value.and_then(|v| v.as_table())) {
            (toml::Value::Table(inner), Some(inner_defaults)) => {
                path.push(key.clone());
                diff_into(doc, account, path, inner, inner_defaults)?;
                path.pop();
            }
            (toml::Value::Table(inner), None) => {
                // A whole group the defaults do not mention: every value in it is an
                // override.
                path.push(key.clone());
                let empty = toml::value::Table::new();
                diff_into(doc, account, path, inner, &empty)?;
                path.pop();
            }
            _ => {
                path.push(key.clone());
                if default_value != Some(value) {
                    set_at(doc, account, path, value)?;
                } else {
                    unset_at(doc, account, path);
                }
                path.pop();
            }
        }
    }
    Ok(())
}

/// Descend to (creating as needed) the table holding the last path segment.
fn table_at<'a>(
    doc: &'a mut DocumentMut,
    account: &str,
    path: &[String],
    create: bool,
) -> Option<&'a mut Table> {
    let mut table = account_table(doc, account)?;
    for segment in &path[..path.len() - 1] {
        if create {
            table = table
                .entry(segment)
                .or_insert_with(|| {
                    let mut new = Table::new();
                    // Not implicit, so it renders as `[accounts.alice.spend.favor]`
                    // rather than an inline table -- a file the panel has written
                    // still looks like one a person would write.
                    new.set_implicit(false);
                    Item::Table(new)
                })
                .as_table_mut()?;
        } else {
            table = table.get_mut(segment)?.as_table_mut()?;
        }
    }
    Some(table)
}

fn set_at(
    doc: &mut DocumentMut,
    account: &str,
    path: &[String],
    value: &toml::Value,
) -> Result<()> {
    let rendered = to_edit_value(value)
        .with_context(|| format!("{} is not a value TOML can hold inline", path.join(".")))?;
    let key = path.last().expect("a path always ends in a key").clone();
    let Some(table) = table_at(doc, account, path, true) else {
        bail!("[accounts.{account}] is not a table");
    };
    table[key.as_str()] = Item::Value(rendered);
    Ok(())
}

fn unset_at(doc: &mut DocumentMut, account: &str, path: &[String]) {
    let key = path.last().expect("a path always ends in a key").clone();
    if let Some(table) = table_at(doc, account, path, false) {
        table.remove(&key);
    }
}

fn set_account_key(doc: &mut DocumentMut, account: &str, key: &str, value: Value) {
    if let Some(table) = account_table(doc, account) {
        table[key] = Item::Value(value);
    }
}

fn unset_account_key(doc: &mut DocumentMut, account: &str, key: &str) {
    if let Some(table) = account_table(doc, account) {
        table.remove(key);
    }
}

/// Drop `[accounts.x.attack]` once its last override is gone, so the file does not
/// accumulate empty headings.
fn prune_empty_sections(doc: &mut DocumentMut, account: &str, _sections: &[String]) {
    if let Some(table) = account_table(doc, account) {
        prune(table);
    }
}

/// Drop any sub-table left with nothing in it, innermost first, so removing the last
/// override under `[spend.favor]` takes the heading with it rather than leaving an
/// empty section behind.
fn prune(table: &mut Table) {
    let names: Vec<String> = table
        .iter()
        .filter(|(_, item)| item.is_table())
        .map(|(name, _)| name.to_string())
        .collect();
    for name in names {
        if let Some(inner) = table.get_mut(&name).and_then(Item::as_table_mut) {
            prune(inner);
            if inner.is_empty() {
                table.remove(&name);
            }
        }
    }
}

fn to_edit_value(value: &toml::Value) -> Option<Value> {
    Some(match value {
        toml::Value::String(s) => Value::from(s.as_str()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::from(*b),
        toml::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(to_edit_value(item)?);
            }
            Value::Array(array)
        }
        // Settings hold no dates and no nested tables; anything else is a bug here
        // rather than something to coerce.
        toml::Value::Datetime(_) | toml::Value::Table(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const FILE: &str = r#"
# A comment that must survive.
[defaults.attack]
max_enemy_dodge = 60.0
delay_secs = 20

[accounts.alice]
[accounts.alice.attack]
max_enemy_dodge = 25.0   # alice is picky
"#;

    fn settings_of(text: &str, account: &str) -> Settings {
        Config::from_str(text)
            .unwrap()
            .accounts
            .into_iter()
            .find(|a| a.name == account)
            .unwrap()
            .settings
    }

    #[test]
    fn a_value_differing_from_the_defaults_is_written_as_an_override() {
        let mut settings = settings_of(FILE, "alice");
        settings.attack.delay_secs = 45;

        let out = apply(FILE, "alice", &settings, true).unwrap();
        assert!(out.contains("delay_secs = 45"), "{out}");
        // And the file is still a file, not a dump of a struct.
        assert!(out.contains("# A comment that must survive."), "{out}");
        // Round-trips: what was written parses back to what was asked for.
        assert_eq!(settings_of(&out, "alice").attack.delay_secs, 45);
    }

    #[test]
    fn a_value_dragged_back_onto_the_default_loses_its_override() {
        let mut settings = settings_of(FILE, "alice");
        assert_eq!(settings.attack.max_enemy_dodge, 25.0);
        settings.attack.max_enemy_dodge = 60.0;

        let out = apply(FILE, "alice", &settings, true).unwrap();
        assert!(!out.contains("25.0"), "the override should be gone:\n{out}");
        // The section had one override and now has none, so the heading goes too.
        assert!(!out.contains("[accounts.alice.attack]"), "{out}");
        // The effective value is still 60, now inherited rather than restated.
        assert_eq!(settings_of(&out, "alice").attack.max_enemy_dodge, 60.0);
    }

    #[test]
    fn disabling_an_account_writes_the_flag_and_enabling_removes_it() {
        let settings = settings_of(FILE, "alice");

        let disabled = apply(FILE, "alice", &settings, false).unwrap();
        assert!(disabled.contains("enabled = false"), "{disabled}");
        assert!(!Config::from_str(&disabled).unwrap().accounts[0].enabled);

        let enabled = apply(&disabled, "alice", &settings, true).unwrap();
        assert!(!enabled.contains("enabled = false"), "{enabled}");
        assert!(Config::from_str(&enabled).unwrap().accounts[0].enabled);
    }

    #[test]
    fn lists_and_enums_survive_the_round_trip() {
        let mut settings = settings_of(FILE, "alice");
        settings.spend.enabled = true;
        settings.boss.planets = vec!["Oceana".into(), "Drakon".into()];
        settings.boss.order = crate::config::BossOrder::Listed;

        let out = apply(FILE, "alice", &settings, true).unwrap();
        let back = settings_of(&out, "alice");
        assert!(back.spend.enabled);
        assert_eq!(back.boss.planets, settings.boss.planets);
        assert_eq!(back.boss.order, crate::config::BossOrder::Listed);
    }

    #[test]
    fn an_unknown_account_is_refused_rather_than_created() {
        let settings = settings_of(FILE, "alice");
        let err = apply(FILE, "mallory", &settings, true)
            .unwrap_err()
            .to_string();
        // Named precisely. `contains("mallory")` also matched the error raised further
        // down when the write itself failed, so deleting the guard left this test
        // green -- which mutation testing caught.
        assert!(
            err.contains("no [accounts.mallory] section"),
            "expected the up-front refusal, got: {err}"
        );

        // And with nothing to write at all -- every value equal to the defaults, so no
        // later step could fail -- the guard is the only thing that can refuse.
        let defaults_only = crate::config::Config::default_settings(FILE).unwrap();
        let err = apply(FILE, "mallory", &defaults_only, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no [accounts.mallory] section"), "{err}");
    }

    #[test]
    fn every_group_of_settings_is_written_not_just_the_listed_ones() {
        // Guards the reason the hand-kept `SECTIONS` list was deleted: the writer
        // walks whatever `Settings` serializes to, so every group is reachable. If a
        // group stopped being written, the round-trip below would lose its value.
        let mut settings = settings_of(FILE, "alice");
        settings.attack.candidate_limit = 33;
        settings.claim.min_scrap = 7.5;
        settings.quest.delay_secs = 61;
        settings.boss.max_flux_per_fight = 9.5;
        settings.spend.max_per_cycle = 4;

        let out = apply(FILE, "alice", &settings, true).unwrap();
        let back = settings_of(&out, "alice");
        assert_eq!(back.attack.candidate_limit, 33);
        assert_eq!(back.claim.min_scrap, 7.5);
        assert_eq!(back.quest.delay_secs, 61);
        assert_eq!(back.boss.max_flux_per_fight, 9.5);
        assert_eq!(back.spend.max_per_cycle, 4);
    }

    #[test]
    fn editing_one_account_leaves_the_others_alone() {
        let text = format!("{FILE}\n[accounts.bob]\n[accounts.bob.attack]\ndelay_secs = 99\n");
        let mut settings = settings_of(&text, "alice");
        settings.attack.delay_secs = 45;

        let out = apply(&text, "alice", &settings, true).unwrap();
        assert_eq!(settings_of(&out, "bob").attack.delay_secs, 99);
    }
}

#[cfg(test)]
mod nested_tests {
    use super::*;
    use crate::config::Config;

    const FILE: &str = "[accounts.alice]\n";

    /// Goals live in sub-tables, so the writer has to descend. Without that it
    /// refuses the value outright and no spend setting can be saved from the panel.
    #[test]
    fn a_nested_goal_setting_round_trips() {
        let mut settings = Config::default_settings(FILE).unwrap();
        settings.spend.enabled = true;
        settings.spend.engineering.weight = 5.0;
        settings.spend.favor.max_scrap_per_crit_point = 250_000.0;
        settings.spend.stake.absorb_surplus = true;

        let out = apply(FILE, "alice", &settings, true).expect("nested settings must be writable");
        let back = Config::from_str(&out).unwrap().accounts.remove(0).settings;
        assert!(back.spend.enabled);
        assert_eq!(back.spend.engineering.weight, 5.0);
        assert_eq!(back.spend.favor.max_scrap_per_crit_point, 250_000.0);
        assert!(back.spend.stake.absorb_surplus);
    }
}
