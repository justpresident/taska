//! `ta create` — append a `Create` event for a new task.

use serde_json::Value;

use crate::cli::{canonicalize_fields, materialize, parse_field_ops};
use crate::config::WorkflowConfig;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType, STATUS_KEY};
use crate::schema::{coerce_event_fields, vet_events, FieldOps};
use crate::storage::EventStore;

pub fn cmd_create(
    store: &impl EventStore,
    workflow: &WorkflowConfig,
    id: &str,
    fields: &[String],
) -> Result<(), DynError> {
    // On a new task the field is absent, so `+=` (append) is just the initial
    // value — fold the append map into the Create payload. `-=` has nothing to
    // remove from yet.
    let FieldOps {
        set: mut payload,
        append,
        subtract,
        mut raw,
    } = parse_field_ops(fields)?;
    if let Some(key) = subtract.keys().next() {
        return Err(format!(
            "`{key}-=…` is meaningless on create: a new task has nothing to remove from"
        )
        .into());
    }
    for (k, v) in append {
        payload.insert(k, v);
    }
    // Display names map onto their canonical storage keys before anything is
    // stamped or vetted (so the event stores `status` whatever the field is
    // called on screen); `raw` keeps its keys aligned for the coercion below.
    canonicalize_fields(&mut payload, workflow)?;
    canonicalize_fields(&mut raw, workflow)?;
    // Stamp the configured default status unless the caller named the status
    // field themselves (even as JSON `null`, the explicit-unset convention) or
    // defaults are turned off with an empty `default_status`.
    if !workflow.default_status.is_empty() && !payload.contains_key(STATUS_KEY) {
        payload.insert(
            STATUS_KEY.to_string(),
            Value::String(workflow.default_status.clone()),
        );
    }
    // And the schema defaults of the declared task type (same convention: an
    // explicit value — or null — in the payload wins over the default).
    let stamps = crate::schema::schema_default_stamps(
        None,
        &payload,
        &std::collections::BTreeSet::default(),
        store.config(),
    );
    for (key, value) in stamps {
        payload.insert(key, value);
    }
    // Verify-then-append under the store lock: schema-aware coercion first,
    // then vetting — which rejects a duplicate `create` (the task already
    // exists) atomically, so two concurrent creates can't both win. A create
    // is never a no-op, so on success it always wrote.
    let draft = MutationEvent::new(OpType::Create, id, payload);
    let config = store.config().clone();
    store.append_checked(&|baseline, log| {
        let state = materialize(&config, baseline, log);
        let mut events = vec![draft.clone()];
        coerce_event_fields(&mut events, &raw, &state, &config);
        vet_events(&events, &state, &config)
    })?;
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
