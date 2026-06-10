//! `ta undo` — reverse the last N events (truncate local, compensate committed).
//!
//! The plan (what changes, and the resulting log) comes from
//! [`crate::action::undo`]; this file renders the preview, runs the confirm, and
//! reports.

use crate::action::undo::{apply, plan};
use crate::cli::confirm;
use crate::error::DynError;
use crate::model::TaskState;
use crate::storage::FileStore;

/// Undo the last `count` event(s) in the log.
///
/// Two paths, chosen by whether any undone event is already git-committed:
/// truncate the log's tail (safe for still-local events; `--remove` extends it to
/// committed ones at the cost of rewriting shared history, with a loud warning),
/// or keep committed history intact and *append* compensating events that walk
/// the state back to the target. Only events still in the log can be undone;
/// anything folded into the baseline by compaction is out of reach.
pub fn cmd_undo(
    store: &FileStore,
    count: usize,
    force: bool,
    remove: bool,
) -> Result<(), DynError> {
    let Some(undo) = plan(store, count, remove)? else {
        println!("Nothing to undo.");
        return Ok(());
    };

    // PREVIEW: name each undone event, then show each affected task's before/after.
    println!("Undoing {} event(s):", undo.count);
    for event in &undo.undone {
        println!("  seq {}: {:?} `{}`", event.seq, event.op, event.task_id);
    }
    for change in &undo.changes {
        println!(
            "  - {}: {} -> {}",
            change.id,
            describe(change.before.as_ref()),
            describe(change.after.as_ref())
        );
    }

    if !confirm("Apply this undo?", force)? {
        println!("Aborted; nothing changed.");
        return Ok(());
    }

    apply(store, &undo)?;
    if undo.rewrites_committed_history {
        eprintln!(
            "DANGER: --remove deleted committed event(s), rewriting shared history. \
             Other branches will see a removal on merge; only do this if you are sure \
             the removed events were never pushed or pulled elsewhere."
        );
    }
    println!("Undone.");
    Ok(())
}

/// Render a task's salient state for the before/after preview: `(absent)` for a
/// missing task, else the JSON of its custom fields plus `deps=<typed map>` when
/// it has any edge — the same `{type: [targets…]}` shape the `deps` column
/// shows. Mirrors the field-centric framing of the other task views.
fn describe(task: Option<&TaskState>) -> String {
    task.map_or_else(
        || "(absent)".to_string(),
        |t| {
            let fields =
                serde_json::to_string(&t.custom_fields).unwrap_or_else(|_| "{}".to_string());
            if t.relationships.is_empty() {
                fields
            } else {
                let deps =
                    serde_json::to_string(&t.relationships).unwrap_or_else(|_| "{}".to_string());
                format!("{fields} deps={deps}")
            }
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::task;

    #[test]
    fn describe_renders_absent_fields_and_typed_deps() {
        assert_eq!(describe(None), "(absent)");
        let mut t = task("a", &["d1"], &[("status", serde_json::json!("open"))]);
        t.relationships
            .insert("relates_to".to_string(), vec!["d2".to_string()]);
        let out = describe(Some(&t));
        assert!(out.contains(r#""status":"open""#), "fields: {out}");
        assert!(
            out.contains(r#"deps={"depends_on":["d1"],"relates_to":["d2"]}"#),
            "typed deps map shown: {out}"
        );

        let no_deps = task("b", &[], &[]);
        assert_eq!(describe(Some(&no_deps)), "{}", "empty fields, no deps");
    }
}
