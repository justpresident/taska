//! `ta edit <id>` (alias `ed`) — round-trip a task's fields through `$EDITOR`.
//!
//! The task's stored fields are serialized to a temp file (TOML by default,
//! JSON with `--json`), opened in the editor, and the saved result is diffed
//! against what was shown: a changed or added field becomes a `set`, a deleted
//! field becomes an unset (the JSON-`null` convention). The write then funnels
//! through the exact `update` path (`build_field_events` + `vet_events`), so
//! schema coercion, heal-on-write defaults, no-op dropping, and reserved-name
//! rejection all apply.
//!
//! On any failure — TOML/JSON syntax, a canonical-name conflict, or a schema
//! violation from the write gate — the message prints to stderr (same as
//! `ta update`) and the user is offered to re-edit *the same file, exactly as
//! they saved it*; declining discards the edit. Relationships are out of scope
//! (managed by `ta dep`): only the scalar/array custom fields are shown.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value};

use crate::cli::{canonicalize_fields, confirm, replay};
use crate::config::{Config, WorkflowConfig};
use crate::error::DynError;
use crate::model::TaskState;
use crate::schema::{build_field_events, canonical_field_pairs, vet_events, FieldOps};
use crate::storage::EventStore;

pub fn cmd_edit(store: &impl EventStore, id: &str, as_json: bool) -> Result<(), DynError> {
    // RAW snapshot (canonical keys, no injected timestamps/computed columns), so
    // only the task's actually-stored fields are editable.
    let snapshot = replay(store, store.load_baseline()?, store.load_mutations()?);
    let task = snapshot.get(id).ok_or_else(|| format!("no task `{id}`"))?;
    let view = to_display(&task.custom_fields, &store.config().workflow);
    let initial = serialize_fields(&view, as_json)?;

    let ext = if as_json { "json" } else { "toml" };
    let tmp = TempFile::create(id, ext, &initial)?;
    eprintln!(
        "editing `{id}` — save to apply, delete a field to unset it, save unchanged for no-op."
    );

    // Re-edit loop: open, validate the saved result, and on any error offer to
    // reopen the same file (left exactly as the user saved it) or discard.
    let config = store.config();
    let payload = loop {
        open_in_editor(&tmp.path)?;
        let edited = std::fs::read_to_string(&tmp.path)?;
        if edited.trim().is_empty() {
            println!("Empty file — discarded; `{id}` left unchanged.");
            return Ok(());
        }
        match preview(&edited, as_json, &view, &snapshot, id, config) {
            Ok(set) => break set,
            Err(e) => {
                eprintln!("error: {e}");
                if !confirm("Re-edit to fix it? (no = discard your changes)", false)? {
                    println!("Discarded; `{id}` left unchanged.");
                    return Ok(());
                }
            }
        }
    };

    if payload.is_empty() {
        println!("No changes — `{id}` left unchanged.");
        return Ok(());
    }

    // Re-validate against current state (the editor ran outside the lock) and
    // append through the shared write path.
    crate::action::write::update(store, id, &set_only(payload))?;
    println!("Updated task `{id}`");
    Ok(())
}

/// Validate a saved edit against the pre-edit snapshot and return the canonical
/// `set` payload (changed/added fields, plus removed fields as `null`). Runs the
/// same parse → diff → canonicalize → build → vet pipeline the final write does,
/// so the user sees the real diagnostics *before* the store lock is taken.
fn preview(
    edited: &str,
    as_json: bool,
    view: &Map<String, Value>,
    snapshot: &HashMap<String, TaskState>,
    id: &str,
    config: &Config,
) -> Result<Map<String, Value>, DynError> {
    let parsed = parse_fields(edited, as_json)?;
    let mut set = diff_payload(view, &parsed);
    canonicalize_fields(&mut set, &config.workflow)?;
    let ops = set_only(set.clone());
    let events = build_field_events(id, &ops, snapshot, config)?;
    vet_events(&events, snapshot, config)?;
    Ok(set)
}

/// A `FieldOps` carrying only `set` fields — edit never appends or subtracts.
fn set_only(set: Map<String, Value>) -> FieldOps {
    FieldOps {
        set,
        append: Map::new(),
        subtract: Map::new(),
        raw: Map::new(),
    }
}

/// Render a task's stored fields under their configured DISPLAY names (the
/// inverse of `canonicalize_fields`), so the editor shows `status`/`type` as the
/// user knows them rather than the canonical storage keys.
fn to_display(custom_fields: &Map<String, Value>, workflow: &WorkflowConfig) -> Map<String, Value> {
    let mut view = custom_fields.clone();
    for (display, canonical) in canonical_field_pairs(workflow) {
        if display == canonical {
            continue;
        }
        if let Some(value) = view.remove(canonical) {
            view.insert(display.clone(), value);
        }
    }
    view
}

/// Serialize fields for the editor: pretty JSON, or TOML (the default). A task
/// whose value can't be represented in TOML (e.g. an `any` field holding a
/// table after a scalar) yields a clear `--json` hint rather than a cryptic
/// serializer error.
fn serialize_fields(fields: &Map<String, Value>, as_json: bool) -> Result<String, DynError> {
    if as_json {
        Ok(serde_json::to_string_pretty(fields)?)
    } else {
        toml::to_string_pretty(fields).map_err(|e| {
            format!("cannot represent this task as TOML ({e}); edit it with `--json` instead")
                .into()
        })
    }
}

/// Parse the saved file back into a field map, surfacing the format's own
/// line/column diagnostics on a syntax error.
fn parse_fields(text: &str, as_json: bool) -> Result<Map<String, Value>, DynError> {
    if as_json {
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}").into())
    } else {
        toml::from_str(text).map_err(|e| format!("invalid TOML: {e}").into())
    }
}

/// Diff the edited fields against what was shown: a new or changed field becomes
/// a set; a field that was present but is now gone becomes an unset (`null`, the
/// replay-time remove convention).
fn diff_payload(view: &Map<String, Value>, edited: &Map<String, Value>) -> Map<String, Value> {
    let mut set = Map::new();
    for (key, value) in edited {
        if view.get(key) != Some(value) {
            set.insert(key.clone(), value.clone());
        }
    }
    for key in view.keys() {
        if !edited.contains_key(key) {
            set.insert(key.clone(), Value::Null);
        }
    }
    set
}

/// Launch `$VISUAL`, else `$EDITOR`, else `vi`, on `path`. The editor string is
/// split on whitespace so `EDITOR="code -w"` passes its flags through.
fn open_in_editor(path: &Path) -> Result<(), DynError> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| "no editor configured (set $EDITOR or $VISUAL)".to_string())?;
    let status = Command::new(program)
        .args(parts)
        .arg(path)
        .status()
        .map_err(|e| format!("failed to launch editor `{editor}`: {e}"))?;
    if !status.success() {
        return Err(format!("editor `{editor}` exited with an error; aborting").into());
    }
    Ok(())
}

/// A temp file that removes itself on drop, so every exit path (success, abort,
/// `?`) cleans up.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn create(id: &str, ext: &str, contents: &str) -> Result<Self, DynError> {
        let safe: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let name = format!("taska-edit-{safe}-{}.{ext}", std::process::id());
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, contents)?;
        Ok(Self { path })
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use serde_json::json;

    fn map(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn toml_round_trips_scalars_and_arrays() {
        // Serialize -> parse must be value-stable, so an UNTOUCHED field diffs to
        // nothing (the whole no-op story depends on this).
        let fields = map(&[
            ("title", json!("Ship it")),
            ("priority", json!(3)),
            ("ratio", json!(1.5)),
            ("done", json!(false)),
            ("tags", json!(["a", "b"])),
        ]);
        let text = serialize_fields(&fields, false).unwrap();
        let back = parse_fields(&text, false).unwrap();
        assert_eq!(fields, back);
        assert!(
            diff_payload(&fields, &back).is_empty(),
            "round-trip is a no-op"
        );
    }

    #[test]
    fn json_round_trips_and_is_stable() {
        let fields = map(&[
            ("title", json!("x")),
            ("n", json!(-2)),
            ("tags", json!(["z"])),
        ]);
        let text = serialize_fields(&fields, true).unwrap();
        let back = parse_fields(&text, true).unwrap();
        assert_eq!(fields, back);
        assert!(diff_payload(&fields, &back).is_empty());
    }

    #[test]
    fn diff_detects_change_add_and_remove() {
        let view = map(&[
            ("title", json!("old")),
            ("priority", json!(1)),
            ("drop", json!("x")),
        ]);
        let edited = map(&[
            ("title", json!("new")), // changed
            ("priority", json!(1)),  // unchanged -> skipped
            ("added", json!(9)),     // new
                                     // "drop" removed
        ]);
        let set = diff_payload(&view, &edited);
        assert_eq!(set.get("title"), Some(&json!("new")));
        assert_eq!(set.get("added"), Some(&json!(9)));
        assert_eq!(set.get("drop"), Some(&Value::Null), "removed field -> null");
        assert!(
            !set.contains_key("priority"),
            "unchanged field is not written"
        );
    }

    #[test]
    fn syntax_errors_are_surfaced() {
        assert!(parse_fields("this is = = not toml", false).is_err());
        assert!(parse_fields("{not json", true).is_err());
    }

    #[test]
    fn to_display_renames_canonical_keys() {
        // Default config renames the canonical `task_type` to the display `type`.
        let workflow = WorkflowConfig::default();
        let stored = map(&[
            (crate::model::TASK_TYPE_KEY, json!("task")),
            ("title", json!("x")),
        ]);
        let view = to_display(&stored, &workflow);
        assert_eq!(view.get(&workflow.type_field), Some(&json!("task")));
        assert!(!view.contains_key(crate::model::TASK_TYPE_KEY));
    }
}
