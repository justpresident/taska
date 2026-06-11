//! `prime` action: a config-tailored snapshot of the store's conventions.
//!
//! The data behind `ta prime` — this store's workflow vocabulary (status field
//! and values, the type field), its declared task types and relationship types,
//! the displayed columns, and a count summary. Gathered as typed [`PrimeFacts`]
//! the frontend renders into an agent primer (the CLI builds a markdown guide;
//! `--format json` serializes the facts as-is). Tailored, not static: it reads
//! THIS store's config, so a renamed status field or a freshly declared type
//! shows up immediately.

use serde::Serialize;

use crate::action::status::status_summary;
use crate::action::{read, StatusSummary, Warning};
use crate::config::Config;
use crate::error::DynError;
use crate::storage::EventStore;

/// One declared field of a task type, distilled for the primer.
#[derive(Serialize, Debug, Clone)]
pub struct FieldFacts {
    pub name: String,
    /// The declared kind (`string`, `enum`, `uint`, `set<string>`, …).
    pub kind: String,
    pub required: bool,
    /// Declared enum values (empty for non-enum fields).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// The declared default value, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// One declared task type.
#[derive(Serialize, Debug, Clone)]
pub struct TypeFacts {
    pub name: String,
    /// Whether reaching `done_status` closes a task of this type (the `closed` flag).
    pub closed: bool,
    pub fields: Vec<FieldFacts>,
}

/// One declared relationship type.
#[derive(Serialize, Debug, Clone)]
pub struct RelationshipFacts {
    pub name: String,
    /// `blocker`, `hierarchy`, or `info`.
    pub kind: String,
    /// The reverse-edge name (empty = one-way, not surfaced on the target).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub inverse: String,
}

/// Headline counts (a distilled [`StatusSummary`]).
#[derive(Serialize, Debug, Clone)]
pub struct SummaryFacts {
    pub total: usize,
    pub open: usize,
    pub ready: usize,
    pub blocked: usize,
    pub closed: usize,
}

/// The config-tailored snapshot behind `ta prime`.
///
/// This store's workflow vocabulary, declared types/relationships, displayed
/// columns, and a count summary — enough for an agent to drive `ta` against THIS
/// store. Serializes directly for `ta prime --format json`.
#[derive(Serialize, Debug, Clone)]
pub struct PrimeFacts {
    /// The display name of the status field (`[workflow] status_field`).
    pub status_field: String,
    pub default_status: String,
    pub done_status: String,
    /// The declared status enum values, if the status field is declared an enum
    /// (else empty — the store accepts free-form status strings).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub statuses: Vec<String>,
    /// The display name of the type field (`[workflow] type_field`).
    pub type_field: String,
    /// `allow` | `warn` | `deny`.
    pub untyped_tasks: String,
    pub task_types: Vec<TypeFacts>,
    pub relationships: Vec<RelationshipFacts>,
    pub columns: Vec<String>,
    pub summary: SummaryFacts,
}

/// A `prime` read: the facts plus any read [`Warning`]s.
pub struct PrimeOutcome {
    pub facts: PrimeFacts,
    pub warnings: Vec<Warning>,
}

/// Gather the config-tailored prime snapshot for `store`.
pub fn prime(store: &impl EventStore) -> Result<PrimeOutcome, DynError> {
    let session = read(store)?;
    let config = store.config();
    let blockers = config.relationships.blocker_types();
    let summary = status_summary(&session.state, &config.workflow, &blockers)?;
    Ok(PrimeOutcome {
        facts: build_facts(config, &summary),
        warnings: session.warnings,
    })
}

/// Distill `config` (+ a precomputed `summary`) into the typed facts.
fn build_facts(config: &Config, summary: &StatusSummary) -> PrimeFacts {
    let workflow = &config.workflow;

    let task_types: Vec<TypeFacts> = config
        .task_types
        .types
        .iter()
        .map(|(name, def)| TypeFacts {
            name: name.clone(),
            closed: def.closed,
            fields: def
                .fields
                .iter()
                .map(|(fname, schema)| FieldFacts {
                    name: fname.clone(),
                    kind: schema.kind_str().to_string(),
                    required: schema.required(),
                    values: schema.values().to_vec(),
                    default: schema.default_value().cloned(),
                })
                .collect(),
        })
        .collect();

    // The status enum values: the declared values of the (display-named) status
    // field wherever a type declares it an enum, deduped in first-seen order.
    // Empty when no type constrains the status — a free-form-status store.
    let mut statuses: Vec<String> = Vec::new();
    for def in config.task_types.types.values() {
        if let Some(schema) = def.fields.get(&workflow.status_field) {
            for v in schema.values() {
                if !statuses.contains(v) {
                    statuses.push(v.clone());
                }
            }
        }
    }

    let relationships: Vec<RelationshipFacts> = config
        .relationships
        .types
        .iter()
        .map(|(name, def)| RelationshipFacts {
            name: name.clone(),
            kind: def.kind.as_str().to_string(),
            inverse: def.inverse.clone(),
        })
        .collect();

    PrimeFacts {
        status_field: workflow.status_field.clone(),
        default_status: workflow.default_status.clone(),
        done_status: workflow.done_status.clone(),
        statuses,
        type_field: workflow.type_field.clone(),
        untyped_tasks: workflow.untyped_tasks.as_str().to_string(),
        task_types,
        relationships,
        columns: config.display.columns.clone(),
        summary: SummaryFacts {
            total: summary.total,
            open: summary.total - summary.closed,
            ready: summary.ready,
            blocked: summary.blocked,
            closed: summary.closed,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::{store_with_schema, InMemoryStore};

    #[test]
    fn facts_mirror_a_declared_schema() {
        // With a schema declared, the facts must reflect the type, its status
        // enum, required fields, and the relationship's kind/inverse verbatim.
        let store = store_with_schema();
        let facts = prime(&store).unwrap().facts;

        assert_eq!(facts.status_field, "status");
        assert_eq!(facts.done_status, "closed");
        assert_eq!(facts.default_status, "todo");
        assert_eq!(facts.type_field, "type");
        assert_eq!(facts.statuses, vec!["todo", "in_progress", "closed"]);

        let task = facts
            .task_types
            .iter()
            .find(|t| t.name == "task")
            .expect("the `task` type");
        assert!(task.closed, "the task type closes");
        let title = task
            .fields
            .iter()
            .find(|f| f.name == "title")
            .expect("title field");
        assert!(title.required, "title is required");

        let depends_on = facts
            .relationships
            .iter()
            .find(|r| r.name == "depends_on")
            .expect("depends_on relationship");
        assert_eq!(depends_on.kind, "blocker");
        assert_eq!(depends_on.inverse, "blocks");
    }

    #[test]
    fn facts_are_free_form_without_a_schema() {
        // `Config::default()` declares no task types, so the status is free-form
        // and there are no type facts — the facts must say so, not invent an enum.
        let facts = prime(&InMemoryStore::default()).unwrap().facts;
        assert!(facts.statuses.is_empty(), "no declared status enum");
        assert!(facts.task_types.is_empty(), "no declared types");
    }

    #[test]
    fn summary_counts_open_as_total_minus_closed() {
        use crate::model::{MutationEvent, OpType};
        use serde_json::{Map, Value};

        let store = InMemoryStore::default();
        let typed = |status: &str| {
            let mut m = Map::new();
            m.insert("type".into(), Value::from("task"));
            m.insert("title".into(), Value::from("a title"));
            m.insert("notes".into(), Value::from("some notes"));
            m.insert("status".into(), Value::from(status));
            m
        };
        store
            .append_events(&[
                MutationEvent::new(OpType::Create, "a", typed("todo")),
                MutationEvent::new(OpType::Create, "b", typed("closed")),
            ])
            .unwrap();

        let summary = prime(&store).unwrap().facts.summary;
        assert_eq!(summary.total, 2);
        assert_eq!(summary.closed, 1);
        assert_eq!(summary.open, 1, "open = total - closed");
    }
}
