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

const SECTIONS: [&str; 5] = ["attack", "claim", "quest", "boss", "upgrade"];

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

    for section in SECTIONS {
        let incoming_section = incoming.get(section).and_then(|v| v.as_table());
        let defaults_section = defaults.get(section).and_then(|v| v.as_table());
        let (Some(incoming_section), Some(defaults_section)) = (incoming_section, defaults_section)
        else {
            continue;
        };

        for (key, value) in incoming_section {
            let is_override = defaults_section.get(key) != Some(value);
            if is_override {
                set(&mut doc, account, section, key, value)?;
            } else {
                unset(&mut doc, account, section, key);
            }
        }
    }

    // `enabled = true` is the default, so it is written only when it is false.
    if enabled {
        unset_account_key(&mut doc, account, "enabled");
    } else {
        set_account_key(&mut doc, account, "enabled", Value::from(false));
    }

    prune_empty_sections(&mut doc, account);
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

fn set(
    doc: &mut DocumentMut,
    account: &str,
    section: &str,
    key: &str,
    value: &toml::Value,
) -> Result<()> {
    let value = to_edit_value(value)
        .with_context(|| format!("{section}.{key} is not a value TOML can hold inline"))?;
    let Some(table) = account_table(doc, account) else {
        bail!("[accounts.{account}] is not a table");
    };
    let section_table = table
        .entry(section)
        .or_insert_with(|| {
            let mut new = Table::new();
            // Rendered as `[accounts.alice.attack]` rather than an inline table, so
            // a file the UI has written still looks like one a person would write.
            new.set_implicit(false);
            Item::Table(new)
        })
        .as_table_mut();
    let Some(section_table) = section_table else {
        bail!("[accounts.{account}.{section}] is not a table");
    };
    section_table[key] = Item::Value(value);
    Ok(())
}

fn unset(doc: &mut DocumentMut, account: &str, section: &str, key: &str) {
    if let Some(table) = account_table(doc, account) {
        if let Some(section_table) = table.get_mut(section).and_then(Item::as_table_mut) {
            section_table.remove(key);
        }
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
fn prune_empty_sections(doc: &mut DocumentMut, account: &str) {
    let Some(table) = account_table(doc, account) else {
        return;
    };
    for section in SECTIONS {
        let empty = table
            .get(section)
            .and_then(Item::as_table)
            .is_some_and(Table::is_empty);
        if empty {
            table.remove(section);
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
        settings.upgrade.enabled = true;
        settings.upgrade.stats = vec![crate::config::Stat::Damage, crate::config::Stat::Defense];
        settings.upgrade.order = crate::config::UpgradeOrder::Listed;

        let out = apply(FILE, "alice", &settings, true).unwrap();
        let back = settings_of(&out, "alice");
        assert!(back.upgrade.enabled);
        assert_eq!(back.upgrade.stats, settings.upgrade.stats);
        assert_eq!(back.upgrade.order, crate::config::UpgradeOrder::Listed);
    }

    #[test]
    fn an_unknown_account_is_refused_rather_than_created() {
        let settings = settings_of(FILE, "alice");
        let err = apply(FILE, "mallory", &settings, true).unwrap_err().to_string();
        assert!(err.contains("mallory"), "{err}");
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
