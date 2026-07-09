//! `ta status` - total, per-status, blocked, ready, and closed counts.
//!
//! The counts come from [`crate::action::status`]; this file is just their
//! presentation (the aligned human table and the JSON object). `--current` is a
//! separate, state-free shortcut: it prints the log's high-water `seq` (the
//! cursor `ta watch --since` takes) and skips materialization entirely.

use serde_json::Value;

use crate::action::{status, StatusSummary};
use crate::cli::print_warnings;
use crate::error::DynError;
use crate::format::{emit, sgr, want_color, OutputArgs};
use crate::storage::EventStore;

pub fn cmd_status(
    store: &impl EventStore,
    current: bool,
    output: &OutputArgs,
) -> Result<(), DynError> {
    // `--current`: just the log's high-water `seq` (the cursor `ta watch --since`
    // takes), without materializing state. 0 on an empty log.
    if current {
        let seq = store.load_mutations()?.last().map_or(0, |e| e.seq);
        emit(output, &seq.to_string(), &serde_json::json!({ "seq": seq }));
        return Ok(());
    }
    let outcome = status(store)?;
    print_warnings(&outcome.warnings);
    let human = render_status_human(&outcome.summary, want_color(output.no_color));
    emit(output, &human, &status_value(&outcome.summary));
    Ok(())
}

/// Human summary: an aligned `Total`, a per-status block (sorted, with an
/// `(unset)` bucket last), then the computed `Ready`/`Blocked`/`Closed` lines.
/// Labels are bolded when `color`.
fn render_status_human(s: &StatusSummary, color: bool) -> String {
    // Per-status rows, indented; the no-status bucket sorts last under `(unset)`.
    let mut status_rows: Vec<(String, usize)> = s
        .by_status
        .iter()
        .map(|(k, v)| (format!("  {k}"), *v))
        .collect();
    if s.no_status > 0 {
        status_rows.push(("  (unset)".to_string(), s.no_status));
    }

    // Width over every numeric row so labels and counts line up in one table.
    let summary_rows = [
        ("Ready", s.ready),
        ("Blocked", s.blocked),
        ("Closed", s.closed),
    ];
    let label_w = status_rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .chain(std::iter::once("Total".len()))
        .chain(summary_rows.iter().map(|(l, _)| l.len()))
        .max()
        .unwrap_or(0);
    let count_w = status_rows
        .iter()
        .map(|(_, c)| *c)
        .chain(std::iter::once(s.total))
        .chain(summary_rows.iter().map(|(_, c)| *c))
        .map(|c| c.to_string().len())
        .max()
        .unwrap_or(1);
    let row = |label: &str, count: usize| {
        let label = sgr(&format!("{label:<label_w$}"), "1", color);
        format!("{label}  {count:>count_w$}")
    };

    let mut lines = vec![
        row("Total", s.total),
        String::new(),
        sgr("By status:", "1", color),
    ];
    lines.extend(status_rows.iter().map(|(label, count)| row(label, *count)));
    lines.push(String::new());
    lines.extend(summary_rows.iter().map(|(label, count)| row(label, *count)));
    lines.join("\n")
}

/// The summary as a JSON object (a single value; `emit` renders it for
/// json/jsonl). Keys serialize in sorted order - deterministic for scripting.
fn status_value(s: &StatusSummary) -> Value {
    let by_status: serde_json::Map<String, Value> = s
        .by_status
        .iter()
        .map(|(k, v)| (k.clone(), Value::from(*v)))
        .collect();
    serde_json::json!({
        "total": s.total,
        "by_status": by_status,
        "no_status": s.no_status,
        "ready": s.ready,
        "blocked": s.blocked,
        "closed": s.closed,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // The counts are computed and tested in `action::status`; here we cover the
    // PRESENTATION of a summary - the human table and the JSON object.
    fn sample() -> StatusSummary {
        StatusSummary {
            total: 5,
            by_status: BTreeMap::from([("todo".to_string(), 3), ("closed".to_string(), 1)]),
            no_status: 1,
            ready: 3,
            blocked: 1,
            closed: 1,
        }
    }

    #[test]
    fn human_output_names_the_sections() {
        let human = render_status_human(&sample(), false);
        assert!(human.contains("Total"), "human: {human}");
        assert!(human.contains("By status:"), "human: {human}");
        assert!(human.contains("(unset)"), "no-status bucket shown: {human}");
        assert!(
            human.contains("Ready") && human.contains("Blocked"),
            "{human}"
        );
    }

    #[test]
    fn json_output_is_one_object_with_the_fields() {
        let parsed = status_value(&sample());
        assert_eq!(parsed["total"], 5);
        assert_eq!(parsed["ready"], 3);
        assert_eq!(parsed["blocked"], 1);
        assert_eq!(parsed["closed"], 1);
        assert_eq!(parsed["no_status"], 1);
        assert_eq!(parsed["by_status"]["todo"], 3);
    }
}
