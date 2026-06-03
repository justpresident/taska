//! `ta show <id>` — a single task in full (every field it has, by default).

use crate::cli::state_of;
use crate::config::DisplayConfig;
use crate::error::DynError;
use crate::format::{full_columns, render_rows, DisplayArgs};
use crate::storage::EventStore;

/// Show a single task by id, defaulting to ALL of its fields (unlike `list`,
/// which uses the configured columns). An explicit `--columns` still restricts;
/// it renders via the same human/json/jsonl path as `list`.
pub fn cmd_show(
    store: &impl EventStore,
    id: &str,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let task = state.get(id).ok_or_else(|| format!("no task `{id}`"))?;
    let tasks = [task];
    // Default to the full task: every field of this one task. An explicit
    // `--columns` overrides; either way the shared `render_rows` dispatch prints.
    let columns = display
        .columns
        .clone()
        .unwrap_or_else(|| full_columns(&tasks, cfg));
    println!("{}", render_rows(&tasks, &columns, display, cfg));
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
        assert!(cmd_show(&store, "api", &d, &DisplayConfig::default()).is_ok());
        assert!(
            cmd_show(&store, "nope", &d, &DisplayConfig::default()).is_err(),
            "unknown id must error"
        );
    }
}
