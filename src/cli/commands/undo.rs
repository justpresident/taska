//! `ta undo` - walk back through real history (truncate local, compensate the rest).
//!
//! The plan (which events, what changes, and the resulting log) comes from
//! [`crate::action::undo`]; this file renders the preview, runs the confirm, and
//! reports.

use std::collections::BTreeSet;

use crate::action::undo::{apply, plan};
use crate::cli::confirm;
use crate::error::DynError;
use crate::format::{render_list_record, sgr, want_color, RowStyle};
use crate::model::{TaskState, DEPS_KEY, STATUS_KEY};
use crate::storage::{EventStore, FileStore};

/// Undo event(s), walking back through real history.
///
/// `seq` targets a specific event (default: the most recent undoable one);
/// `count` undoes that many, going older, skipping anything already undone.
/// Applying truncates the clean uncommitted tail of the selection (safe for
/// still-local events; `--remove` extends truncation to committed ones at the
/// cost of rewriting shared history, with a loud warning) and *appends*
/// compensating events - each marked `_undoes=<seq>` - for committed or buried
/// targets. Only events still in the log can be undone; anything folded into the
/// baseline by compaction is out of reach.
pub fn cmd_undo(
    store: &FileStore,
    seq: Option<u64>,
    count: usize,
    force: bool,
    remove: bool,
) -> Result<(), DynError> {
    let Some(undo) = plan(store, seq, count, remove)? else {
        println!("Nothing to undo.");
        return Ok(());
    };

    // PREVIEW: name each undone event, then a per-task colored diff of ONLY the
    // changed columns/deps - the current state's lines marked `-` (red), the
    // reverted state's `+` (green), rendered through `show`'s record renderer so
    // the cell coloring matches list/show exactly.
    println!("Undoing {} event(s):", undo.count);
    for event in &undo.undone {
        println!("  seq {}: {:?} `{}`", event.seq, event.op, event.task_id);
    }
    let color = want_color(false);
    let config = store.config();
    let blockers = config.relationships.blocker_types();
    // undo's before/after are CANONICAL state, so key the style on the canonical
    // status field (not its display name) for correct status/done coloring.
    let style = RowStyle {
        status_field: STATUS_KEY,
        done_status: &config.workflow.done_status,
    };
    for change in &undo.changes {
        let cols = changed_columns(change.before.as_ref(), change.after.as_ref());
        let before = diff_block(
            change.before.as_ref(),
            &cols,
            "- ",
            "31",
            color,
            &blockers,
            style,
        );
        let after = diff_block(
            change.after.as_ref(),
            &cols,
            "+ ",
            "32",
            color,
            &blockers,
            style,
        );
        if before.is_none() && after.is_none() {
            continue;
        }
        println!("{}:", change.id);
        if let Some(b) = before {
            println!("{b}");
        }
        if let Some(a) = after {
            println!("{a}");
        }
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
    let seq = undo.last_seq();
    println!("[seq:{seq}] Undone.");
    Ok(())
}

/// The custom-field and `deps` columns whose value differs between two task
/// states - what the undo will actually change. A missing state (a create/delete
/// being reversed) contributes all of the other side's columns.
fn changed_columns(before: Option<&TaskState>, after: Option<&TaskState>) -> Vec<String> {
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for t in [before, after].into_iter().flatten() {
        keys.extend(t.custom_fields.keys().map(String::as_str));
    }
    let mut cols: Vec<String> = keys
        .into_iter()
        .filter(|k| {
            before.and_then(|s| s.custom_fields.get(*k))
                != after.and_then(|s| s.custom_fields.get(*k))
        })
        .map(str::to_string)
        .collect();
    if before.map(|s| &s.relationships) != after.map(|s| &s.relationships) {
        cols.push(DEPS_KEY.to_string());
    }
    cols
}

/// Render the changed columns a single state actually carries as a marked diff
/// block: each [`render_list_record`] line prefixed with `marker` in SGR `code`.
/// `None` when this side has none of the changed columns (e.g. a field only the
/// other side has, so it shows on just one of the `-`/`+` blocks).
fn diff_block(
    state: Option<&TaskState>,
    changed: &[String],
    marker: &str,
    code: &str,
    color: bool,
    blockers: &BTreeSet<String>,
    style: RowStyle,
) -> Option<String> {
    let task = state?;
    let cols: Vec<String> = changed
        .iter()
        .filter(|c| {
            if c.as_str() == DEPS_KEY {
                !task.relationships.is_empty()
            } else {
                task.custom_fields.contains_key(c.as_str())
            }
        })
        .cloned()
        .collect();
    if cols.is_empty() {
        return None;
    }
    let m = sgr(marker, code, color);
    Some(
        render_list_record(task, &cols, color, blockers, style)
            .lines()
            .map(|l| format!("  {m}{l}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::names::*;
    use crate::test_support::{task, task_rel};

    fn style() -> RowStyle<'static> {
        RowStyle {
            status_field: STATUS_KEY,
            done_status: DONE_STATUS,
        }
    }

    #[test]
    fn changed_columns_reports_differing_fields_and_deps() {
        let before = task(
            "a",
            &[],
            &[
                (STATUS_KEY, serde_json::json!("open")),
                ("owner", serde_json::json!("x")),
            ],
        );
        let after = task(
            "a",
            &[],
            &[
                (STATUS_KEY, serde_json::json!("closed")),
                ("owner", serde_json::json!("x")),
            ],
        );
        // Only the status value differs; `owner` is identical and there are no deps.
        assert_eq!(
            changed_columns(Some(&before), Some(&after)),
            vec![STATUS_KEY.to_string()]
        );

        // A created task (no `before`) contributes all its columns, deps included.
        let created = task_rel(
            "a",
            BLOCKER,
            &["d"],
            &[(STATUS_KEY, serde_json::json!("open"))],
        );
        let mut cols = changed_columns(None, Some(&created));
        cols.sort();
        assert_eq!(cols, vec![DEPS_KEY.to_string(), STATUS_KEY.to_string()]);
    }

    #[test]
    fn diff_block_marks_only_the_columns_each_side_carries() {
        let before = task_rel(
            "a",
            BLOCKER,
            &["d"],
            &[(STATUS_KEY, serde_json::json!("closed"))],
        );
        let after = task("a", &[], &[(STATUS_KEY, serde_json::json!("open"))]); // deps removed
        let changed = changed_columns(Some(&before), Some(&after));
        let blockers = BTreeSet::new();

        // `before` carries status + deps, both marked `-`.
        let b = diff_block(
            Some(&before),
            &changed,
            "- ",
            "31",
            false,
            &blockers,
            style(),
        )
        .unwrap();
        assert!(
            b.contains(&format!("- {STATUS_KEY}: closed")) && b.contains(&format!("- {DEPS_KEY}:")),
            "before block: {b}"
        );
        // `after` carries only status (deps gone), so no deps line on the `+` side.
        let a = diff_block(
            Some(&after),
            &changed,
            "+ ",
            "32",
            false,
            &blockers,
            style(),
        )
        .unwrap();
        assert!(
            a.contains(&format!("+ {STATUS_KEY}: open")) && !a.contains(DEPS_KEY),
            "after block, no deps: {a}"
        );
    }
}
