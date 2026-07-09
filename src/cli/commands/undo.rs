//! `ta undo` - walk back through real history (truncate local, compensate the rest).
//!
//! The plan (which events, what changes, and the resulting log) comes from
//! [`crate::action::undo`]; this file renders the preview, runs the confirm, and
//! reports.

use crate::action::undo::{apply, plan};
use crate::cli::confirm;
use crate::error::DynError;
use crate::format::{render_state_diff, want_color};
use crate::storage::FileStore;

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

    // PREVIEW: name each undone event, then a per-task diff of only the lines that
    // change - removed lines in red `-`, restored lines in green `+` - via the
    // shared `render_state_diff` (the same view `watch` prints).
    println!("Undoing {} event(s):", undo.count);
    for event in &undo.undone {
        println!("  seq {}: {:?} `{}`", event.seq, event.op, event.task_id);
    }
    let color = want_color(false);
    for change in &undo.changes {
        let diff = render_state_diff(change.before.as_ref(), change.after.as_ref(), color);
        if diff.is_empty() {
            continue;
        }
        println!("{}:", change.id);
        println!("{diff}");
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
