//! `ta resolve` — clear a surfaced merge conflict and prune orphaned events.
//!
//! The data work (parsing the marker, finding orphans, clearing/pruning) lives in
//! [`crate::action::resolve`]; this file renders the report and runs the confirm.

use crate::action::resolve::{apply, plan, ConflictItem, OrphanEvent};
use crate::cli::confirm;
use crate::error::DynError;
use crate::storage::FileStore;

/// Clean up after a merge or a divergent history: report and clear a surfaced
/// merge conflict, and drop any orphaned events the log has accumulated.
///
/// The deterministic merge is already written to the log by the driver, so the
/// conflict step only acknowledges the conflicts and removes the marker; per-field
/// resolution is future work. The orphan step prunes events that apply to nothing
/// (a dropped `Create` left their target missing) — state-neutral, but confirmed
/// first. With neither a marker nor an orphan, there is nothing to do.
pub fn cmd_resolve(store: &FileStore, force: bool) -> Result<(), DynError> {
    let resolution = plan(store)?;
    render_conflicts(resolution.conflicts.as_deref());

    // The orphan prune is the only step the user confirms; clearing the marker
    // just acknowledges the already-written merge.
    let mut drop_orphans = false;
    if !resolution.orphans.is_empty() {
        render_orphans(&resolution.orphans);
        drop_orphans = confirm("Drop these orphaned events from the log?", force)?;
        if !drop_orphans {
            println!("Aborted; the log is unchanged.");
        }
    }

    let (cleared, dropped) = apply(store, &resolution, drop_orphans)?;
    if dropped > 0 {
        println!("Dropped {dropped} orphaned event(s) from the log.");
    }
    if !cleared && dropped == 0 {
        println!("Nothing to resolve (no merge conflicts and no orphaned events).");
    }
    Ok(())
}

/// Print the tentatively-merged conflicts from the marker (nothing for no marker).
fn render_conflicts(conflicts: Option<&[ConflictItem]>) {
    let Some(items) = conflicts else {
        return;
    };
    if items.is_empty() {
        println!("Merge marker present but lists no conflicts.");
        return;
    }
    println!(
        "{} field conflict(s) were merged tentatively (keeping ours):",
        items.len()
    );
    for item in items {
        let (task, ours, theirs, kept) = (&item.task_id, &item.ours, &item.theirs, &item.kept);
        match &item.field {
            Some(f) => {
                println!("  - `{task}`.{f}: ours={ours} / theirs={theirs} -> kept {kept}");
            }
            None => {
                println!("  - `{task}` (whole task): ours={ours} / theirs={theirs} -> kept {kept}");
            }
        }
    }
    println!(
        "\nThe tentative merge is already written to the log. To accept it, `git add` \
         the files and commit; to pick differently, edit the log or re-merge with a \
         different `on_conflict` strategy."
    );
}

/// Name every orphaned event that would be dropped, before touching the log.
fn render_orphans(orphans: &[OrphanEvent]) {
    println!(
        "{} orphaned event(s) apply to no existing task and would be dropped:",
        orphans.len()
    );
    for o in orphans {
        println!("  - seq {}: {:?} `{}`", o.seq, o.op, o.task_id);
    }
}
