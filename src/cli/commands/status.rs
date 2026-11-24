//! `ta status` - total, per-status, blocked, ready, and closed counts, plus the
//! log's high-water `seq`.
//!
//! The counts come from [`crate::action::status`]; this file is just their
//! presentation (the aligned human table and the JSON object), with the read's
//! high-water `seq` as a trailing `Seq` line / `seq` field. Two state-free
//! shortcuts skip materialization: `--current` prints ONLY that `seq` (a log
//! position: what `ta history --since` and `ta undo --seq` speak), and `--cursor`
//! prints ONLY the watch cursor (this clone's highest arrival number, see
//! [`crate::arrival`]) - the value `ta watch --since` takes. They differ once a
//! merge has restacked the log.

use serde_json::Value;

use crate::action::{status, StatusSummary};
use crate::cli::render_warnings;
use crate::error::DynError;
use crate::format::{render_args, sgr, want_color, OutputArgs};
use crate::storage::EventStore;

pub fn cmd_status(
    store: &impl EventStore,
    current: bool,
    cursor: bool,
    output: &OutputArgs,
) -> Result<(), DynError> {
    // `--current`: just the log's high-water `seq` (a log position, NOT a watch
    // cursor), without materializing state. 0 on an empty log.
    if current {
        let seq = store.load_mutations()?.last().map_or(0, |e| e.seq);
        for line in render_args(
            output,
            || seq.to_string(),
            || serde_json::json!({ "seq": seq }),
        ) {
            println!("{line}");
        }
        return Ok(());
    }
    // `--cursor`: just the watch cursor - this clone's highest arrival number -
    // for a watcher that starts without writing first. 0 on an empty log.
    if cursor {
        let log = store.load_mutations()?;
        let cursor = store.cursor(&log)?;
        for line in render_args(
            output,
            || cursor.to_string(),
            || serde_json::json!({ "cursor": cursor }),
        ) {
            println!("{line}");
        }
        return Ok(());
    }
    let outcome = status(store)?;
    for warning in render_warnings(&outcome.warnings) {
        eprintln!("{warning}");
    }
    let color = want_color(output.no_color);
    for line in render_args(
        output,
        || render_status_human(&outcome.summary, outcome.seq, color),
        || status_to_json_value(&outcome.summary, outcome.seq),
    ) {
        println!("{line}");
    }
    Ok(())
}

/// Human summary: an aligned `Total`, a per-status block (sorted, with an
/// `(unset)` bucket last), the computed `Ready`/`Blocked`/`Closed` lines, then the
/// log's high-water `Seq` (a log position, not a watch cursor). Labels are
/// bolded when `color`.
fn render_status_human(s: &StatusSummary, seq: u64, color: bool) -> String {
    // Per-status rows, indented; the no-status bucket sorts last under `(unset)`.
    let mut status_rows: Vec<(String, usize)> = s
        .by_status
        .iter()
        .map(|(k, v)| (format!("  {k}"), *v))
        .collect();
    if s.no_status > 0 {
        status_rows.push(("  (unset)".to_string(), s.no_status));
    }

    // Width over every numeric row so labels and counts line up in one table. The
    // `Seq` value can be the widest number, so it joins the count-width max.
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
        .chain(std::iter::once("Seq".len()))
        .max()
        .unwrap_or(0);
    let count_w = status_rows
        .iter()
        .map(|(_, c)| *c)
        .chain(std::iter::once(s.total))
        .chain(summary_rows.iter().map(|(_, c)| *c))
        .map(|c| c.to_string().len())
        .max()
        .unwrap_or(1)
        .max(seq.to_string().len());
    let row = |label: &str, value: &str| {
        let label = sgr(&format!("{label:<label_w$}"), "1", color);
        format!("{label}  {value:>count_w$}")
    };

    let mut lines = vec![
        row("Total", &s.total.to_string()),
        String::new(),
        sgr("By status:", "1", color),
    ];
    lines.extend(
        status_rows
            .iter()
            .map(|(label, count)| row(label, &count.to_string())),
    );
    lines.push(String::new());
    lines.extend(
        summary_rows
            .iter()
            .map(|(label, count)| row(label, &count.to_string())),
    );
    lines.push(String::new());
    lines.push(row("Seq", &seq.to_string()));
    lines.join("\n")
}

/// The summary as a JSON object (a single value; `render_args` renders it for
/// json/jsonl). `seq` is the log's high-water seq (not a watch cursor). Keys serialize in sorted
/// order - deterministic for scripting.
fn status_to_json_value(s: &StatusSummary, seq: u64) -> Value {
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
        "seq": seq,
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
        let human = render_status_human(&sample(), 357, false);
        assert!(human.contains("Total"), "human: {human}");
        assert!(human.contains("By status:"), "human: {human}");
        assert!(human.contains("(unset)"), "no-status bucket shown: {human}");
        assert!(
            human.contains("Ready") && human.contains("Blocked"),
            "{human}"
        );
        assert!(
            human.contains("Seq") && human.contains("357"),
            "seq: {human}"
        );
    }

    #[test]
    fn json_output_is_one_object_with_the_fields() {
        let parsed = status_to_json_value(&sample(), 357);
        assert_eq!(parsed["total"], 5);
        assert_eq!(parsed["ready"], 3);
        assert_eq!(parsed["blocked"], 1);
        assert_eq!(parsed["closed"], 1);
        assert_eq!(parsed["no_status"], 1);
        assert_eq!(parsed["by_status"]["todo"], 3);
        assert_eq!(parsed["seq"], 357);
    }
}
