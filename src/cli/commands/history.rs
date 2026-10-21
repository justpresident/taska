//! `ta history` - the per-event log of matching tasks, oldest-first.
//!
//! A thin shell over [`crate::action::history`]: each event renders as a
//! `[seq:N] <time> <op> <id>` header plus that event's `-`/`+` lines (the
//! shared `format::state_diff` view `watch` and `undo` print), closed by the
//! cursor - the `--since` a following `ta watch` continues from.

use serde_json::{json, Value};

use crate::action::history::{history, op_label, HistoryEntry, HistoryQuery};
use crate::error::DynError;
use crate::format::{
    render_args, render_diff_lines, seq_tag, state_diff, task_ref, want_color, OutputArgs,
};
use crate::storage::EventStore;

pub fn cmd_history(
    store: &impl EventStore,
    query: &HistoryQuery,
    output: &OutputArgs,
) -> Result<(), DynError> {
    let outcome = history(store, query)?;
    if query.since.is_none() {
        if let Some(through) = outcome.compacted_through {
            eprintln!(
                "taska: note: events up to seq {through} were compacted into the baseline; \
                 history starts at seq {}",
                through + 1
            );
        }
    }
    let color = want_color(output.no_color);
    for line in render_args(
        output,
        || to_human(&outcome.entries, outcome.cursor, color),
        || to_json(&outcome.entries, outcome.cursor),
    ) {
        println!("{line}");
    }
    Ok(())
}

/// An entry's notes: what it undoes, or the merge strategy that wrote it.
fn annotation(entry: &HistoryEntry) -> String {
    let mut notes = Vec::new();
    if let Some(seq) = entry.undoes {
        notes.push(format!("undoes seq {seq}"));
    }
    if let Some(strategy) = &entry.merge_strategy {
        notes.push(format!("merge resolution: {strategy}"));
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!("  ({})", notes.join("; "))
    }
}

/// Header + diff per entry, blank-line separated, closed by the cursor.
fn to_human(entries: &[HistoryEntry], cursor: u64, color: bool) -> String {
    if entries.is_empty() {
        return format!("(no history)\n{}", seq_tag(cursor, color));
    }
    let blocks: Vec<String> = entries
        .iter()
        .map(|e| {
            let header = format!(
                "{} {}  {}  {}{}",
                seq_tag(e.seq, color),
                e.timestamp.format("%Y-%m-%d %H:%M:%S"),
                op_label(&e.op),
                task_ref(&e.id, color),
                annotation(e)
            );
            let (removed, added) = state_diff(e.before.as_ref(), e.after.as_ref());
            if removed.is_empty() && added.is_empty() {
                format!("{header}\n  (no visible change)")
            } else {
                format!("{header}\n{}", render_diff_lines(&removed, &added, color))
            }
        })
        .collect();
    // A blank line keeps the closing cursor from reading as part of the last
    // entry, whose header also starts with a `[seq:N]`.
    format!("{}\n\n{}", blocks.join("\n\n"), seq_tag(cursor, color))
}

/// `{cursor, events: [{seq, timestamp, op, id, removed, added, undoes?, merge?}]}`.
fn to_json(entries: &[HistoryEntry], cursor: u64) -> Value {
    let events: Vec<Value> = entries
        .iter()
        .map(|e| {
            let (removed, added) = state_diff(e.before.as_ref(), e.after.as_ref());
            let mut event = json!({
                "seq": e.seq,
                "timestamp": e.timestamp.to_rfc3339(),
                "op": op_label(&e.op),
                "id": &e.id,
                "removed": removed,
                "added": added,
            });
            if let Some(seq) = e.undoes {
                event["undoes"] = json!(seq);
            }
            if let Some(strategy) = &e.merge_strategy {
                event["merge"] = json!(strategy);
            }
            event
        })
        .collect();
    json!({ "cursor": cursor, "events": events })
}
