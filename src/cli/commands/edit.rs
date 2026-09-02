//! `ta edit <id>` (alias `ed`) - round-trip a task's fields through `$EDITOR`.
//!
//! A thin shell over [`action::edit`]: that layer builds the form and decides
//! what a save means; this one serializes it to a temp file (TOML by default,
//! JSON with `--json`), launches the editor, and turns the outcome into messages.
//! With `--create`, an absent id starts from the same form and an existing one is
//! rejected before the editor opens.
//!
//! On any failure - TOML/JSON syntax, a canonical-name conflict, or a schema
//! violation from the write gate - the message prints to stderr (same as
//! `ta update`) and the user is offered to re-edit *the same file, exactly as
//! they saved it*; declining, saving it empty, or answering EOF discards the
//! edit. Relationships are out of scope (managed by `ta dep`): only the
//! scalar/array custom fields are shown.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value};

use crate::action::edit::{EditForm, EditMode, Preview};
use crate::cli::{confirm, confirm_with_default};
use crate::error::DynError;
use crate::storage::EventStore;

pub fn cmd_edit(
    store: &impl EventStore,
    id: &str,
    create_missing: bool,
    as_json: bool,
) -> Result<(), DynError> {
    let config = store.config();
    let form = EditForm::open(store, id, create_missing)?;
    let mode = form.mode();
    let initial = serialize_fields(&form.template, as_json)?;

    let ext = if as_json { "json" } else { "toml" };
    let tmp = TempFile::create(id, ext, &initial)?;
    eprintln!("{}", opening(mode, id));

    // Re-edit loop: open, validate the saved result, and on any error offer to
    // reopen the same file (left exactly as the user saved it) or discard. A save
    // that introduces a brand-new field name (not used by any task) is treated
    // interactively here - add it, or re-edit to fix a typo - rather than blocked
    // with `--new-field` the way `create`/`update` are.
    let (payload, allow_new_fields) = loop {
        open_in_editor(&tmp.path)?;
        let saved = std::fs::read_to_string(&tmp.path)?;
        let outcome =
            parse_fields(&saved, as_json).and_then(|parsed| form.preview(&parsed, config));
        match outcome {
            Ok(Preview::Empty) => {
                println!("Empty file - discarded; {}", untouched(mode, id));
                return Ok(());
            }
            Ok(Preview::Ready { set, new_fields }) if new_fields.is_empty() => break (set, false),
            Ok(Preview::Ready { set, new_fields }) => {
                eprintln!(
                    "note: this adds field name(s) no task uses yet: {}",
                    new_fields.join(", ")
                );
                if confirm(
                    "Add them as new fields? (no = re-edit to fix a typo)",
                    false,
                )? {
                    break (set, true);
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                if !confirm_with_default("Re-edit to fix it? (no = discard your changes)", true)? {
                    println!("Discarded; {}", untouched(mode, id));
                    return Ok(());
                }
            }
        }
    };

    if payload.is_empty() {
        println!("{}", nothing_written(mode, id));
        return Ok(());
    }

    // Re-validate against current state (the editor ran outside the lock) and
    // append through the shared write path.
    let outcome = form.apply(store, payload, allow_new_fields)?;
    // The gate can still drop every event as a no-op (a value another writer
    // already set, say), and a write that didn't happen has no seq to report.
    let Some(last) = outcome.written.last() else {
        println!("{}", nothing_written(mode, id));
        return Ok(());
    };
    let seq = last.seq;
    match mode {
        EditMode::Update => println!("[seq:{seq}] Updated task `{id}`"),
        EditMode::Create => println!("[seq:{seq}] Created task `{id}`"),
    }
    Ok(())
}

/// The line printed before the editor opens - what a save will do.
fn opening(mode: EditMode, id: &str) -> String {
    match mode {
        EditMode::Update => format!(
            "editing `{id}` - save to apply, delete a field to unset it, save unchanged for no-op."
        ),
        EditMode::Create => {
            format!(
                "creating `{id}` - save fields to create it, save empty or unchanged to discard."
            )
        }
    }
}

/// The tail of every discard message: what happened to the task (nothing).
fn untouched(mode: EditMode, id: &str) -> String {
    match mode {
        EditMode::Update => format!("`{id}` left unchanged."),
        EditMode::Create => format!("`{id}` not created."),
    }
}

/// The message for a save that resolved to no write at all.
fn nothing_written(mode: EditMode, id: &str) -> String {
    let reason = match mode {
        EditMode::Update => "No changes",
        EditMode::Create => "No fields saved",
    };
    format!("{reason} - {}", untouched(mode, id))
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
        assert_eq!(parse_fields(&text, false).unwrap(), fields);
    }

    #[test]
    fn json_round_trips_and_is_stable() {
        let fields = map(&[
            ("title", json!("x")),
            ("n", json!(-2)),
            ("tags", json!(["z"])),
        ]);
        let text = serialize_fields(&fields, true).unwrap();
        assert_eq!(parse_fields(&text, true).unwrap(), fields);
    }

    #[test]
    fn a_comments_only_save_parses_to_no_fields() {
        // What makes it a discard rather than a create: the document carries no
        // fields, which `action::edit` reads as `Preview::Empty`.
        assert!(parse_fields("# nothing here\n", false).unwrap().is_empty());
        assert!(parse_fields("{}", true).unwrap().is_empty());
    }

    #[test]
    fn syntax_errors_are_surfaced() {
        assert!(parse_fields("this is = = not toml", false).is_err());
        assert!(parse_fields("{not json", true).is_err());
    }
}
