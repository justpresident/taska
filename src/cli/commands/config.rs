//! `ta config get|set|list` — view or change `.taska/config.toml` by dotted key.

use clap::Subcommand;

use crate::cli::state_of;
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
    /// Check the config against the task graph: `ta config validate`
    Validate,
}

/// Dispatch `ta config get|set|list|validate`.
pub fn cmd_config(store: &FileStore, action: ConfigAction) -> Result<(), DynError> {
    match action {
        ConfigAction::Get { key } => cmd_config_get(store.config(), &key),
        ConfigAction::List => cmd_config_list(store.config()),
        ConfigAction::Set { key, value } => cmd_config_set(store, &key, &value),
        ConfigAction::Validate => cmd_config_validate(store),
    }
}

/// Validate the effective config against the materialized task graph, reporting
/// every problem found (or confirming it's clean). Run this after hand-editing
/// `.taska/config.toml`; `config set` runs the same check before persisting.
/// Schema NON-conformance of existing tasks is listed as warnings, not errors —
/// grandfathered data is read-tolerated by design, and failing here would block
/// declaring a schema over an existing store at all.
fn cmd_config_validate(store: &FileStore) -> Result<(), DynError> {
    let state = state_of(store)?;
    store.config().validate_against(&state)?;
    // The conformance report needs RAW state (canonical keys, no injected
    // timestamps) — the display view above would skew the check.
    let raw = crate::cli::replay(store, store.load_baseline()?, store.load_mutations()?);
    let report = crate::cli::schema_conformance_report(&raw, store.config());
    for line in &report {
        eprintln!("warning: {line}");
    }
    if report.is_empty() {
        println!("Config OK ({} task(s) checked).", state.len());
    } else {
        println!(
            "Config OK ({} task(s) checked; {} not conforming to their task-type schema — \
             writes to those tasks must bring them into conformance).",
            state.len(),
            report.len()
        );
    }
    Ok(())
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

    // Guard against typo'd keys: serde(default) silently drops an unknown field,
    // so the value must survive a load round-trip to confirm the key is real.
    let normalized = toml::Value::try_from(&candidate)?;
    let mut cur = &normalized;
    for part in key.split('.') {
        cur = cur.get(part).ok_or_else(|| {
            format!("unknown config key `{key}` (no such field; nothing was changed)")
        })?;
    }

    // Reject the edit unless the resulting config is valid against the current
    // task graph — the keep_events floor, bad enums, plus relationship/cycle
    // consistency. The commands you'd use to fix a graph problem run the cheap
    // struct-only `validate`, so this never locks you out.
    let state = state_of(store)?;
    candidate.validate_against(&state)?;

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
/// The walk is table-LIKE, not table-only: an intermediate may be a `[section]`
/// or an inline table (`column_max_width = { title = 80 }`, the relationship
/// defs), and both must remain settable.
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
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for &parent in parents {
        if table.get(parent).is_none() {
            table.insert(parent, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        table = table
            .get_mut(parent)
            .and_then(toml_edit::Item::as_table_like_mut)
            .ok_or_else(|| format!("config key `{parent}` is not a table"))?;
    }
    let mut item = toml_edit::Item::Value(value);
    // Replacing an existing value: carry its decor over (the spacing inside an
    // inline table), and swap the item IN PLACE — `insert` would re-create the
    // key and silently drop the comment attached to it.
    if let Some(old) = table.get(last).and_then(toml_edit::Item::as_value) {
        if let Some(new) = item.as_value_mut() {
            *new.decor_mut() = old.decor().clone();
        }
    }
    if table.get(last).is_some() {
        if let Some(existing) = table.get_mut(last) {
            *existing = item;
        }
    } else {
        table.insert(last, item);
    }
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
    fn set_dotted_descends_into_inline_tables() {
        // The template styles column_max_width and the relationship defs as
        // inline tables — a dotted set must walk INTO them (table-like, not
        // table-only) and keep the inline style.
        let mut doc = "[display]\ncolumn_max_width = { title = 80 }\n\
                       [relationships]\ndepends_on = { kind = \"blocker\", inverse = \"blocks\" }\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        set_dotted(
            &mut doc,
            "display.column_max_width.title",
            parse_config_value("120"),
        )
        .unwrap();
        set_dotted(
            &mut doc,
            "relationships.depends_on.kind",
            parse_config_value("\"hierarchy\""),
        )
        .unwrap();
        let text = doc.to_string();
        assert!(
            text.contains("column_max_width = { title = 120 }"),
            "updated in place, still inline: {text}"
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.display.column_max_width.get("title"), Some(&120));
        assert_eq!(
            cfg.relationships.types["depends_on"].kind,
            crate::config::RelKind::Hierarchy
        );
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
