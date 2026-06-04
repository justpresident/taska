//! `ta config get|set|list` — view or change `.taska/config.toml` by dotted key.

use clap::Subcommand;

use crate::config::Config;
use crate::error::DynError;
use crate::storage::{EventStore, FileStore};

/// `ta config` subcommands: git-config-style get/set/list over dotted keys.
#[derive(Subcommand)]
pub enum ConfigAction {
    /// Print one effective value: `ta config get compaction.keep_events`
    Get { key: String },
    /// Set a value, validating the result: `ta config set merge.on_conflict ours`
    Set { key: String, value: String },
    /// Print every effective config value as `dotted.key = value`
    List,
}

/// Dispatch `ta config get|set|list`.
pub fn cmd_config(store: &FileStore, action: ConfigAction) -> Result<(), DynError> {
    match action {
        ConfigAction::Get { key } => cmd_config_get(store.config(), &key),
        ConfigAction::List => cmd_config_list(store.config()),
        ConfigAction::Set { key, value } => cmd_config_set(store, &key, &value),
    }
}

/// Print one effective config value addressed by a dotted key. Reads the merged
/// config (file values over defaults), so a key absent from the file still
/// resolves to its default.
fn cmd_config_get(cfg: &Config, key: &str) -> Result<(), DynError> {
    let root = toml::Value::try_from(cfg)?;
    let mut cur = &root;
    for part in key.split('.') {
        cur = cur
            .get(part)
            .ok_or_else(|| format!("no config key `{key}`"))?;
    }
    println!("{}", show_config_value(cur));
    Ok(())
}

/// Print every effective config value as sorted `dotted.key = value` lines.
fn cmd_config_list(cfg: &Config) -> Result<(), DynError> {
    let root = toml::Value::try_from(cfg)?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    flatten_config("", &root, &mut pairs);
    pairs.sort();
    for (k, v) in pairs {
        println!("{k} = {v}");
    }
    Ok(())
}

/// Set one config value, preserving the file's comments, then validate the
/// result before writing — so an invalid edit (unknown key, bad type or enum,
/// `keep_events` below the floor) is rejected and the file left untouched.
fn cmd_config_set(store: &FileStore, key: &str, raw: &str) -> Result<(), DynError> {
    let path = store.base_dir.join("config.toml");
    // Edit the existing file, or seed from the documented template when absent,
    // so even a first `set` on a fresh store yields a fully-commented config.
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::config::default_toml(),
        Err(e) => return Err(e.into()),
    };
    let mut doc = existing.parse::<toml_edit::DocumentMut>()?;
    let value = parse_config_value(raw);
    let shown = value.to_string().trim().to_string();
    set_dotted(&mut doc, key, value)?;

    // Reject the change unless the whole document still deserializes to a valid
    // Config (catches bad types / unknown enum variants) AND passes validate()
    // (catches semantic limits like the keep_events floor).
    let candidate: Config = toml::from_str(&doc.to_string())?;
    candidate.validate()?;

    // Guard against typo'd keys: serde(default) silently drops an unknown field,
    // so the value must survive a load round-trip to confirm the key is real.
    let normalized = toml::Value::try_from(&candidate)?;
    let mut cur = &normalized;
    for part in key.split('.') {
        cur = cur.get(part).ok_or_else(|| {
            format!("unknown config key `{key}` (no such field; nothing was changed)")
        })?;
    }

    std::fs::write(&path, doc.to_string())?;
    println!("Set {key} = {shown}");
    Ok(())
}

/// Render a leaf config value git-config-style: bare for strings, TOML form
/// (numbers, bools, arrays) otherwise.
fn show_config_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Flatten a TOML tree into sorted-friendly `dotted.key`/value pairs, recursing
/// through tables so nested sub-tables (e.g. `display.column_max_width.*`) show.
fn flatten_config(prefix: &str, v: &toml::Value, out: &mut Vec<(String, String)>) {
    if let toml::Value::Table(table) = v {
        for (k, val) in table {
            let key = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            flatten_config(&key, val, out);
        }
    } else {
        out.push((prefix.to_string(), show_config_value(v)));
    }
}

/// Coerce a CLI string into a TOML value using TOML's own value grammar (so
/// `100` becomes an integer, `true` a bool, `["a","b"]` an array, `"x"` a
/// string). A bare word that isn't valid TOML — e.g. `open` — falls back to a
/// string, matching how `create`/`update` coerce field values.
fn parse_config_value(raw: &str) -> toml_edit::Value {
    format!("__x__ = {raw}")
        .parse::<toml_edit::DocumentMut>()
        .ok()
        .and_then(|doc| {
            doc.get("__x__")
                .and_then(toml_edit::Item::as_value)
                .cloned()
        })
        .unwrap_or_else(|| toml_edit::Value::from(raw.to_string()))
}

/// Set a dotted key in a `toml_edit` document, creating intermediate tables as
/// needed. Editing in place preserves the surrounding comments and formatting,
/// which is the whole reason `set` uses `toml_edit` rather than re-serializing.
fn set_dotted(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: toml_edit::Value,
) -> Result<(), DynError> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("invalid config key `{key}`").into());
    }
    let (last, parents) = parts
        .split_last()
        .ok_or_else(|| format!("invalid config key `{key}`"))?;
    let mut table = doc.as_table_mut();
    for &parent in parents {
        let item = table
            .entry(parent)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        table = item
            .as_table_mut()
            .ok_or_else(|| format!("config key `{parent}` is not a table"))?;
    }
    table[*last] = toml_edit::Item::Value(value);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;

    #[test]
    fn config_value_parsing_coerces_by_toml_grammar() {
        // Integers, bools and arrays parse to their TOML types; a bare word that
        // isn't valid TOML falls back to a string (like create/update coercion).
        assert!(parse_config_value("100").is_integer());
        assert!(parse_config_value("true").is_bool());
        assert!(parse_config_value(r#"["a","b"]"#).is_array());
        assert!(parse_config_value("open").is_str(), "bare word -> string");
        assert_eq!(parse_config_value("open").as_str(), Some("open"));
    }

    #[test]
    fn set_dotted_updates_in_place_and_creates_nested_tables() {
        let mut doc = "[compaction]\n# keep comment\nkeep_events = 1000\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        // Update an existing key: value changes, the comment survives.
        set_dotted(
            &mut doc,
            "compaction.keep_events",
            parse_config_value("200"),
        )
        .unwrap();
        let text = doc.to_string();
        assert!(text.contains("keep_events = 200"), "updated: {text}");
        assert!(text.contains("# keep comment"), "comment preserved: {text}");

        // A brand-new dotted path creates the intermediate table.
        set_dotted(
            &mut doc,
            "display.column_max_width.title",
            parse_config_value("80"),
        )
        .unwrap();
        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.display.column_max_width.get("title"), Some(&80));

        // An empty key segment is rejected rather than producing a bogus table.
        assert!(set_dotted(&mut doc, "display..title", parse_config_value("1")).is_err());
    }

    #[test]
    fn config_flatten_and_show_render_git_style() {
        let cfg = Config::default();
        let root = toml::Value::try_from(&cfg).unwrap();
        let mut pairs = Vec::new();
        flatten_config("", &root, &mut pairs);
        pairs.sort();
        // Nested sub-tables flatten to dotted keys; strings render bare.
        assert!(pairs.contains(&("workflow.status_field".to_string(), "status".to_string())));
        assert!(pairs.contains(&("compaction.keep_events".to_string(), "5000".to_string())));
        assert!(pairs.contains(&(
            "display.column_max_width.title".to_string(),
            "80".to_string()
        )));
    }
}
