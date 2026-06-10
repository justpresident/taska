//! `ta create` — parse the `key=value` fields, then create via the shared write path.

use crate::cli::{canonicalize_fields, parse_field_ops};
use crate::error::DynError;
use crate::schema::FieldOps;
use crate::storage::EventStore;

pub fn cmd_create(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let workflow = &store.config().workflow;
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
    // Display names map onto their canonical storage keys before the action
    // stamps defaults / vets (so the event stores `status` whatever the field is
    // called on screen); `raw` keeps its keys aligned for the coercion.
    canonicalize_fields(&mut payload, workflow)?;
    canonicalize_fields(&mut raw, workflow)?;
    crate::action::write::create(store, id, payload, &raw)?;
    println!("Created task `{id}`");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::action::read;
    use crate::test_support::InMemoryStore;

    #[test]
    fn create_then_materialize() {
        let store = InMemoryStore::default();
        cmd_create(&store, "api", &["status=open".into(), "priority=3".into()]).unwrap();
        let state = read(&store).unwrap().state;
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
        cmd_create(&store, "api", &[]).unwrap();
        let state = read(&store).unwrap().state;
        assert_eq!(
            state["api"].custom_fields["status"],
            serde_json::json!("todo")
        );
    }

    #[test]
    fn empty_default_status_leaves_task_statusless() {
        // The default status comes from the store's config (the single source of
        // truth), so an empty `default_status` leaves the task statusless.
        let mut store = InMemoryStore::default();
        store.config.workflow.default_status = String::new();
        cmd_create(&store, "api", &[]).unwrap();
        let state = read(&store).unwrap().state;
        assert!(!state["api"].custom_fields.contains_key("status"));
    }

    #[test]
    fn explicit_null_status_opts_out_of_the_default() {
        // `null` is the unset convention; it must suppress the default rather
        // than being overwritten by it (and replay drops the field entirely).
        let store = InMemoryStore::default();
        cmd_create(&store, "api", &["status=null".into()]).unwrap();
        let state = read(&store).unwrap().state;
        assert!(!state["api"].custom_fields.contains_key("status"));
    }

    #[test]
    fn invalid_field_is_rejected() {
        let store = InMemoryStore::default();
        let err = cmd_create(&store, "x", &["no_equals_sign".into()]);
        assert!(err.is_err());
    }
}
