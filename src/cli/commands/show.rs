//! `ta show <id>` — a single task in full (every field it has, by default).

use serde_json::Value;

use crate::cli::{relationship_edges, state_of};
use crate::config::DisplayConfig;
use crate::error::DynError;
use crate::format::{full_columns, render_rows, DisplayArgs};
use crate::model::DEPENDS_ON;
use crate::storage::EventStore;

/// Show a single task by id, defaulting to ALL of its fields (unlike `list`,
/// which uses the configured columns). An explicit `--columns` still restricts.
/// Human output defaults to a readable vertical record (`[display].show_layout`,
/// overridable with `--layout`); `--format json`/`jsonl` go through the same path
/// as `list`.
pub fn cmd_show(
    store: &impl EventStore,
    id: &str,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let mut task = state
        .get(id)
        .cloned()
        .ok_or_else(|| format!("no task `{id}`"))?;
    // Surface the task's typed relationships (forward + inverse-mirrored) as
    // ordinary array fields, so the record and json both show them.
    // Skip `depends_on` — the `deps` built-in already shows it.
    let types = store.config().relationships.types.clone();
    for (name, targets) in relationship_edges(&state, id, &types) {
        if name == DEPENDS_ON {
            continue;
        }
        let arr = targets.into_iter().map(Value::String).collect();
        task.custom_fields.insert(name, Value::Array(arr));
    }
    let tasks = [&task];
    // Default to the full task: every field of this one task. An explicit
    // `--columns` overrides.
    let columns = display
        .columns
        .clone()
        .unwrap_or_else(|| full_columns(&tasks, cfg));
    // Resolve the effective layout (flag, else `[display].show_layout`); json/jsonl
    // ignore it. The same `render_rows` path serves `list` and `show`.
    let mut display = display.clone();
    display.layout = Some(display.layout.unwrap_or(cfg.show_layout));
    println!("{}", render_rows(&tasks, &columns, &display, cfg));
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
