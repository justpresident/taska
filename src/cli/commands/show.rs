//! `ta show <id>...` - one or more tasks in full (every field they have, by
//! default).
//!
//! The tasks (with their inverse edges surfaced) come from [`crate::action::show`];
//! this file is just their presentation.

use crate::action::show;
use crate::cli::print_warnings;
use crate::config::DisplayConfig;
use crate::error::DynError;
use crate::format::{full_columns, render_rows, DisplayArgs, RowStyle};
use crate::storage::EventStore;

/// Show one or more tasks by id, defaulting to ALL of their fields (unlike
/// `list`, which uses the configured columns). An explicit `--columns` still
/// restricts. Human output defaults to a readable vertical record
/// (`[display].show_layout`, overridable with `--layout`), one record per task;
/// `--format json`/`jsonl` go through the same path as `list`.
pub fn cmd_show(
    store: &impl EventStore,
    ids: &[String],
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let outcome = show(store, ids)?;
    print_warnings(&outcome.warnings);
    let tasks: Vec<&_> = outcome.tasks.iter().collect();
    // Default to the full task: every field of these tasks. An explicit
    // `--columns` overrides.
    let columns = display
        .columns
        .clone()
        .unwrap_or_else(|| full_columns(&tasks, cfg));
    // Resolve the effective layout (flag, else `[display].show_layout`); json/jsonl
    // ignore it. The same `render_rows` path serves `list` and `show`.
    let mut display = display.clone();
    display.layout = Some(display.layout.unwrap_or(cfg.show_layout));
    let blockers = store.config().relationships.blocker_types();
    let workflow = &store.config().workflow;
    let style = RowStyle {
        status_field: &workflow.status_field,
        done_status: &workflow.done_status,
    };
    println!(
        "{}",
        render_rows(&tasks, &columns, &display, cfg, &blockers, style)
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::model::{MutationEvent, OpType};
    use crate::test_support::{display, store_without_timestamps};
    use serde_json::Map;

    #[test]
    fn show_renders_known_task_and_errors_on_unknown() {
        let store = store_without_timestamps();
        store
            .append_events(&[MutationEvent::new(OpType::Create, "api", Map::new())])
            .unwrap();

        let d = display(OutputFormat::Human, false, None);
        assert!(cmd_show(&store, &["api".into()], &d, &DisplayConfig::default()).is_ok());
        assert!(
            cmd_show(&store, &["nope".into()], &d, &DisplayConfig::default()).is_err(),
            "unknown id must error"
        );
        // Any unknown id among several still errors.
        assert!(
            cmd_show(
                &store,
                &["api".into(), "nope".into()],
                &d,
                &DisplayConfig::default()
            )
            .is_err(),
            "an unknown id alongside a known one must error"
        );
    }

    #[test]
    fn show_accepts_multiple_ids_deduplicated() {
        let store = store_without_timestamps();
        store
            .append_events(&[
                MutationEvent::new(OpType::Create, "api", Map::new()),
                MutationEvent::new(OpType::Create, "web", Map::new()),
            ])
            .unwrap();

        // Several ids in one call, with a duplicate that should collapse.
        let outcome = show(&store, &["web".into(), "api".into(), "web".into()]).unwrap();
        let ids: Vec<&str> = outcome.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["web", "api"], "deduplicated, first-occurrence order");

        let d = display(OutputFormat::Human, false, None);
        assert!(cmd_show(
            &store,
            &["web".into(), "api".into()],
            &d,
            &DisplayConfig::default()
        )
        .is_ok());
    }
}
