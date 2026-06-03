//! `ta create` — append a `Create` event for a new task.

use serde_json::Value;

use crate::cli::parse_fields;
use crate::config::WorkflowConfig;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_create(
    store: &impl EventStore,
    workflow: &WorkflowConfig,
    id: &str,
    fields: &[String],
) -> Result<(), DynError> {
    let mut payload = parse_fields(fields)?;
    // Stamp the configured default status unless the caller named the status
    // field themselves (even as JSON `null`, the explicit-unset convention) or
    // defaults are turned off with an empty `default_status`.
    if !workflow.default_status.is_empty() && !payload.contains_key(&workflow.status_field) {
        payload.insert(
            workflow.status_field.clone(),
            Value::String(workflow.default_status.clone()),
        );
    }
    store.append_events(&[MutationEvent::new(OpType::Create, id, payload)])?;
    println!("Created task `{id}`");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::cli::state_of;
    use crate::test_support::InMemoryStore;

    #[test]
    fn create_then_materialize() {
        let store = InMemoryStore::default();
        cmd_create(
            &store,
            &WorkflowConfig::default(),
            "api",
            &["status=open".into(), "priority=3".into()],
        )
        .unwrap();
        let state = state_of(&store).unwrap();
        // An explicit status wins over the configured default.
        assert_eq!(
            state["api"].custom_fields["status"],
            serde_json::json!("open")
        );
        // `priority=3` is coerced to a JSON number, not a string.
        assert_eq!(state["api"].custom_fields["priority"], serde_json::json!(3));
    }

    #[test]
    fn bare_create_stamps_default_status() {
        let store = InMemoryStore::default();
        cmd_create(&store, &WorkflowConfig::default(), "api", &[]).unwrap();
        let state = state_of(&store).unwrap();
        assert_eq!(
            state["api"].custom_fields["status"],
            serde_json::json!("todo")
        );
    }

    #[test]
    fn empty_default_status_leaves_task_statusless() {
        let store = InMemoryStore::default();
        let workflow = WorkflowConfig {
            default_status: String::new(),
            ..WorkflowConfig::default()
        };
        cmd_create(&store, &workflow, "api", &[]).unwrap();
        let state = state_of(&store).unwrap();
        assert!(!state["api"].custom_fields.contains_key("status"));
    }

    #[test]
    fn explicit_null_status_opts_out_of_the_default() {
        // `null` is the unset convention; it must suppress the default rather
        // than being overwritten by it (and replay drops the field entirely).
        let store = InMemoryStore::default();
        cmd_create(
            &store,
            &WorkflowConfig::default(),
            "api",
            &["status=null".into()],
        )
        .unwrap();
        let state = state_of(&store).unwrap();
        assert!(!state["api"].custom_fields.contains_key("status"));
    }

    #[test]
    fn invalid_field_is_rejected() {
        let store = InMemoryStore::default();
        let err = cmd_create(
            &store,
            &WorkflowConfig::default(),
            "x",
            &["no_equals_sign".into()],
        );
        assert!(err.is_err());
    }
}
