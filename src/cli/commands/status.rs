//! `ta status` — total, per-status, blocked, ready, and closed counts.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use crate::cli::state_of;
use crate::config::WorkflowConfig;
use crate::error::DynError;
use crate::format::OutputFormat;
use crate::graph;
use crate::model::{is_done, TaskState};
use crate::storage::EventStore;

/// Aggregate counts for `ta status`.
///
/// Status values are user-defined, so the per-status buckets are *discovered*
/// from the data rather than hardcoded — `done_status` is simply the bucket that
/// also feeds the `closed` count. `blocked` and `ready` are COMPUTED from the
/// dependency graph, never read from a status value: `ready` reuses the same set
/// as `ta ready`, and among not-done tasks the two partition the set (a not-done
/// task is blocked iff an existing dependency isn't done, else ready).
struct StatusSummary {
    total: usize,
    by_status: BTreeMap<String, usize>,
    no_status: usize,
    ready: usize,
    blocked: usize,
    closed: usize,
}

fn status_summary(
    state: &HashMap<String, TaskState>,
    workflow: &WorkflowConfig,
    blockers: &BTreeSet<String>,
) -> Result<StatusSummary, DynError> {
    let (field, done) = (&workflow.status_field, &workflow.done_status);
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    let mut no_status = 0usize;
    for t in state.values() {
        match t.custom_fields.get(field) {
            Some(Value::String(s)) => *by_status.entry(s.clone()).or_default() += 1,
            // A non-string status still groups, keyed by its compact JSON form.
            Some(v) => {
                *by_status
                    .entry(serde_json::to_string(v).unwrap_or_default())
                    .or_default() += 1;
            }
            None => no_status += 1,
        }
    }
    let ready = graph::ready_tasks(state, field, done, blockers)?.len();
    let closed = state.values().filter(|t| is_done(t, field, done)).count();
    let blocked = state
        .values()
        .filter(|t| {
            !is_done(t, field, done)
                && graph::blocker_edges(t, blockers)
                    .into_iter()
                    .any(|(d, _)| state.get(d).is_some_and(|dep| !is_done(dep, field, done)))
        })
        .count();
    Ok(StatusSummary {
        total: state.len(),
        by_status,
        no_status,
        ready,
        blocked,
        closed,
    })
}

pub fn cmd_status(
    store: &impl EventStore,
    workflow: &WorkflowConfig,
    format: OutputFormat,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let blockers = store.config().relationships.blocker_types();
    let summary = status_summary(&state, workflow, &blockers)?;
    let out = match format {
        // The summary is a single object, so json and jsonl render identically.
        OutputFormat::Json | OutputFormat::Jsonl => render_status_json(&summary),
        OutputFormat::Human => render_status_human(&summary),
    };
    println!("{out}");
    Ok(())
}

/// Human summary: an aligned `Total`, a per-status block (sorted, with an
/// `(unset)` bucket last), then the computed `Ready`/`Blocked`/`Closed` lines.
fn render_status_human(s: &StatusSummary) -> String {
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
    let row = |label: &str, count: usize| format!("{label:<label_w$}  {count:>count_w$}");

    let mut lines = vec![
        row("Total", s.total),
        String::new(),
        "By status:".to_string(),
    ];
    lines.extend(status_rows.iter().map(|(label, count)| row(label, *count)));
    lines.push(String::new());
    lines.extend(summary_rows.iter().map(|(label, count)| row(label, *count)));
    lines.join("\n")
}

/// Machine-readable summary as a single compact JSON object, keys in a fixed
/// order so the output is stable for scripting.
fn render_status_json(s: &StatusSummary) -> String {
    let by_status: Vec<String> = s
        .by_status
        .iter()
        .map(|(k, v)| format!("{}:{v}", serde_json::to_string(k).unwrap_or_default()))
        .collect();
    format!(
        "{{\"total\":{},\"by_status\":{{{}}},\"no_status\":{},\"ready\":{},\"blocked\":{},\"closed\":{}}}",
        s.total,
        by_status.join(","),
        s.no_status,
        s.ready,
        s.blocked,
        s.closed
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::{state, task};

    #[test]
    fn status_summary_counts_and_partitions_ready_blocked() {
        let workflow = WorkflowConfig::default(); // status / closed
        let tasks = vec![
            task("a", &[], &[("status", serde_json::json!("todo"))]), // ready (no deps)
            task("b", &["a"], &[("status", serde_json::json!("todo"))]), // blocked by a
            task("c", &[], &[("status", serde_json::json!("closed"))]), // done
            task("d", &["c"], &[("status", serde_json::json!("todo"))]), // ready (dep done)
            task("e", &[], &[]),                                      // no status -> ready
        ];
        let st = state(&tasks);
        let blockers = BTreeSet::from(["depends_on".to_string()]);
        let s = status_summary(&st, &workflow, &blockers).unwrap();

        assert_eq!(s.total, 5);
        assert_eq!(s.by_status.get("todo"), Some(&3));
        assert_eq!(s.by_status.get("closed"), Some(&1));
        assert_eq!(s.no_status, 1);
        assert_eq!(s.closed, 1, "one done task");
        assert_eq!(s.ready, 3, "a, d, e");
        assert_eq!(s.blocked, 1, "b");
        // Among not-done tasks, ready and blocked partition the set.
        let not_done = s.total - s.closed;
        assert_eq!(s.ready + s.blocked, not_done);

        // Human output names the sections and a `(unset)` bucket for no-status.
        let human = render_status_human(&s);
        assert!(human.contains("Total"), "human: {human}");
        assert!(human.contains("By status:"), "human: {human}");
        assert!(human.contains("(unset)"), "no-status bucket shown: {human}");
        assert!(
            human.contains("Ready") && human.contains("Blocked"),
            "{human}"
        );

        // JSON output is a single valid object with the computed fields.
        let json = render_status_json(&s);
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["total"], 5);
        assert_eq!(parsed["ready"], 3);
        assert_eq!(parsed["blocked"], 1);
        assert_eq!(parsed["closed"], 1);
        assert_eq!(parsed["no_status"], 1);
        assert_eq!(parsed["by_status"]["todo"], 3);
    }
}
