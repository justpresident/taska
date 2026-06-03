//! `ta create` — append a `Create` event for a new task.

use crate::cli::parse_fields;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_create(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let payload = parse_fields(fields)?;
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
        cmd_create(&store, "api", &["status=open".into(), "priority=3".into()]).unwrap();
        let state = state_of(&store).unwrap();
        assert_eq!(
            state["api"].custom_fields["status"],
            serde_json::json!("open")
        );
        // `priority=3` is coerced to a JSON number, not a string.
        assert_eq!(state["api"].custom_fields["priority"], serde_json::json!(3));
    }

    #[test]
    fn invalid_field_is_rejected() {
        let store = InMemoryStore::default();
        let err = cmd_create(&store, "x", &["no_equals_sign".into()]);
        assert!(err.is_err());
    }
}
