//! `config` action: read (`get`/`list`), validate, and edit (`set`) the store's
//! `config.toml`.
//!
//! `get`/`list` are pure functions of a [`Config`]; `validate` checks it against
//! the materialized graph; `set` edits the file in place (preserving comments via
//! `toml_edit`) and rejects an invalid result. Rendering - the git-config display
//! form - is the frontend's job; these return values and reports.

use crate::action::{materialize, read, Warning};
use crate::config::Config;
use crate::error::DynError;
use crate::schema::schema_conformance_report;
use crate::storage::{EventStore, FileStore};

/// Resolve one effective config value by dotted key (file values over defaults).
pub fn get(cfg: &Config, key: &str) -> Result<toml::Value, DynError> {
    let root = toml::Value::try_from(cfg)?;
    let mut cur = &root;
    for part in key.split('.') {
        cur = cur
            .get(part)
            .ok_or_else(|| format!("no config key `{key}`"))?;
    }
    Ok(cur.clone())
}

/// Every effective config value as `(dotted.key, value)` pairs, sorted by key.
pub fn list(cfg: &Config) -> Result<Vec<(String, toml::Value)>, DynError> {
    let root = toml::Value::try_from(cfg)?;
    let mut pairs: Vec<(String, toml::Value)> = Vec::new();
    flatten("", &root, &mut pairs);
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(pairs)
}

/// A `validate` result.
///
/// The task count checked, the schema-nonconformance report (empty = all
/// conform), and the read warnings the materialization surfaced.
pub struct ValidateReport {
    pub task_count: usize,
    pub nonconformance: Vec<String>,
    pub read_warnings: Vec<Warning>,
}

/// Validate the effective config against the materialized task graph.
///
/// Schema NON-conformance of existing tasks is reported (not errored) -
/// grandfathered data is read-tolerated, and erroring here would block declaring
/// a schema over an existing store at all. The conformance check runs on RAW
/// state (canonical keys, no injected timestamps) so the display view can't skew
/// it.
pub fn validate(store: &FileStore) -> Result<ValidateReport, DynError> {
    let session = read(store)?;
    store.config().validate_against(&session.state)?;
    let raw = materialize(
        store.config(),
        &store.load_baseline()?,
        &store.load_mutations()?,
    );
    let nonconformance = schema_conformance_report(&raw, store.config());
    Ok(ValidateReport {
        task_count: session.state.len(),
        nonconformance,
        read_warnings: session.warnings,
    })
}

/// Set one config value (comment-preserving), validating before writing.
///
/// An invalid edit (unknown key, bad type/enum, `keep_events` below the floor) is
/// rejected and the file left untouched. Returns the value as written, for the
/// frontend's confirmation line.
pub fn set(store: &FileStore, key: &str, raw: &str) -> Result<String, DynError> {
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

    // Reject unless the whole document still deserializes to a valid Config (bad
    // types / unknown enum variants) AND passes validate() (semantic limits like
    // the keep_events floor).
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

    // Reject unless valid against the current task graph (keep_events floor, bad
    // enums, relationship/cycle consistency). The commands you'd use to fix a
    // graph problem run the cheap struct-only `validate`, so this never locks you
    // out.
    candidate.validate_against(&read(store)?.state)?;

    std::fs::write(&path, doc.to_string())?;
    Ok(shown)
}

/// Flatten a TOML tree into `dotted.key`/value pairs, recursing through tables so
/// nested sub-tables (e.g. `display.column_max_width.*`) show.
fn flatten(prefix: &str, v: &toml::Value, out: &mut Vec<(String, toml::Value)>) {
    if let toml::Value::Table(table) = v {
        for (k, val) in table {
            let key = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            flatten(&key, val, out);
        }
    } else {
        out.push((prefix.to_string(), v.clone()));
    }
}

/// Coerce a CLI string into a TOML value using TOML's own value grammar (so
/// `100` becomes an integer, `true` a bool, `["a","b"]` an array, `"x"` a
/// string). A bare word that isn't valid TOML - e.g. `open` - falls back to a
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
    // inline table), and swap the item IN PLACE - `insert` would re-create the
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
    use crate::test_support::names::*;
    use crate::test_support::renamed_config;

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
        // inline tables - a dotted set must walk INTO them (table-like, not
        // table-only) and keep the inline style. Use renamed rel names to avoid
        // hardcoding defaults.
        let doc_str = format!(
            "[display]\ncolumn_max_width = {{ title = 80 }}\n\
             [relationships]\n{BLOCKER} = {{ kind = \"blocker\", inverse = \"{BLOCKER_INV}\" }}\n"
        );
        let mut doc = doc_str.parse::<toml_edit::DocumentMut>().unwrap();
        set_dotted(
            &mut doc,
            "display.column_max_width.title",
            parse_config_value("120"),
        )
        .unwrap();
        set_dotted(
            &mut doc,
            &format!("relationships.{BLOCKER}.kind"),
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
            cfg.relationships.types[BLOCKER].kind,
            crate::config::RelKind::Hierarchy
        );
    }

    // NOTE: left on defaults intentionally - this test SPECIFICALLY verifies
    // the default value of `workflow.status_field` in `Config::default()`.
    #[test]
    fn list_flattens_nested_tables_to_dotted_keys() {
        let pairs = list(&renamed_config()).unwrap();
        let find = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            find("workflow.status_field"),
            Some(toml::Value::from(STATUS_FIELD))
        );
        assert_eq!(
            find("compaction.keep_events"),
            Some(toml::Value::from(5000))
        );
        assert_eq!(
            find("display.column_max_width.title"),
            Some(toml::Value::from(80))
        );
    }
}
