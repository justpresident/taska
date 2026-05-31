//! Domain model: the event schema and the materialized task shape.
//!
//! Pure data with no knowledge of how it is stored, replayed, or displayed.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum OpType {
    Create,
    Update,
    Delete,
    AddDep,
    RemoveDep,
}

/// A single append-only record in the mutation log.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MutationEvent {
    pub id: String,               // Unique UUIDv4 identifier
    pub timestamp: DateTime<Utc>, // ISO 8601 timeline location
    pub op: OpType,
    pub task_id: String,

    // Catch-all for schema-agnostic field management.
    #[serde(flatten)]
    pub payload: Map<String, Value>,
}

impl MutationEvent {
    pub fn new(op: OpType, task_id: impl Into<String>, payload: Map<String, Value>) -> Self {
        MutationEvent {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            op,
            task_id: task_id.into(),
            payload,
        }
    }
}

/// The materialized final state of a single task (lives only in memory, or as a
/// compacted baseline record).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskState {
    pub id: String,
    pub depends_on: Vec<String>,
    pub custom_fields: Map<String, Value>,
}
