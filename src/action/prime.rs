//! `prime` action: a config-tailored snapshot of the store's conventions.
//!
//! The data behind `ta prime` - this store's workflow vocabulary (status field
//! and values, the type field), its declared task types and relationship types,
//! the displayed columns, and a count summary. Gathered as typed [`PrimeFacts`]
//! the frontend renders into an agent primer (the CLI builds a markdown guide;
//! `--format json` serializes the facts as-is). Tailored, not static: it reads
//! THIS store's config, so a renamed status field or a freshly declared type
//! shows up immediately.

use std::collections::BTreeMap;

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
    /// The declared kind (`string`, `enum`, `uint`, `set<string>`, ...).
    pub kind: String,
    pub required: bool,
    /// Declared enum values (empty for non-enum fields).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// The declared default value, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// The declared state machine over an enum field: what each value may
    /// change into (empty = freely settable). An agent that can't see this
    /// only learns the workflow by tripping the write gate.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub transitions: BTreeMap<String, Vec<String>>,
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
/// columns, and a count summary - enough for an agent to drive `ta` against THIS
/// store. Serializes directly for `ta prime --format json`.
#[derive(Serialize, Debug, Clone)]
pub struct PrimeFacts {
    /// The display name of the status field (`[workflow] status_field`).
    pub status_field: String,
    pub default_status: String,
    pub done_status: String,
    /// The declared status enum values, if the status field is declared an enum
    /// (else empty - the store accepts free-form status strings).
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

/// Runnable example tokens derived from the facts.
///
/// A status to claim with, a type + its required fields to create with, a blocker
/// to link, a field to filter on - all config-derived, so every example a
/// frontend weaves from them is runnable against THIS store. Shared by the CLI
/// primer and the `ta init` integration block.
pub struct PrimeExamples {
    /// A representative non-default, non-done status to claim work with.
    pub claim: String,
    /// The first declared type's name (or `task` when none is declared).
    pub type_name: String,
    /// That type's required fields as `name="..."` tokens (minus the status field,
    /// which `create` stamps), space-joined; `title="..."` when none are declared.
    pub req_example: String,
    /// The first gating relationship type (a `dep add` example).
    pub blocker: String,
    /// A ready-to-run `list` filter: an optional enum field if one exists, else a
    /// `not-done` status filter.
    pub filter: String,
}

/// Derive the runnable [`PrimeExamples`] from the facts.
#[must_use]
pub fn examples(f: &PrimeFacts) -> PrimeExamples {
    let sf = &f.status_field;
    let claim = f
        .statuses
        .iter()
        .find(|s| *s != &f.default_status && *s != &f.done_status)
        .unwrap_or(&f.default_status)
        .clone();

    let first_type = f.task_types.first();
    let type_name = first_type.map_or("task", |t| t.name.as_str()).to_string();
    let req_fields: Vec<String> = first_type
        .map(|t| {
            t.fields
                .iter()
                .filter(|x| x.required && x.name != *sf)
                .map(|x| format!("{}=\"...\"", x.name))
                .collect()
        })
        .unwrap_or_default();
    let req_example = if req_fields.is_empty() {
        "title=\"...\"".to_string()
    } else {
        req_fields.join(" ")
    };

    let blocker = f
        .relationships
        .iter()
        .find(|r| r.kind == "blocker" || r.kind == "hierarchy")
        .or_else(|| f.relationships.first())
        .map_or("depends_on", |r| r.name.as_str())
        .to_string();

    let filter = first_type
        .and_then(|t| {
            t.fields
                .iter()
                .find(|x| !x.required && !x.values.is_empty())
        })
        .map_or_else(
            || format!("'{sf}!={}'", f.done_status),
            |x| format!("'{}={}'", x.name, x.values[0]),
        );

    PrimeExamples {
        claim,
        type_name,
        req_example,
        blocker,
        filter,
    }
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
                    transitions: schema.transitions().cloned().unwrap_or_default(),
                })
                .collect(),
        })
        .collect();

    // The status enum values: the declared values of the (display-named) status
    // field wherever a type declares it an enum, deduped in first-seen order.
    // Empty when no type constrains the status - a free-form-status store.
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
    use crate::test_support::names::*;
    use crate::test_support::{store_renamed, store_with_schema};

    #[test]
    fn facts_mirror_a_declared_schema() {
        // With a schema declared, the facts must reflect the type, its status
        // enum, required fields, and the relationship's kind/inverse verbatim -
        // all under the store's RENAMED names (so any hardcoded default fails).
        let facts = prime(&store_with_schema()).unwrap().facts;

        assert_eq!(facts.status_field, STATUS_FIELD);
        assert_eq!(facts.done_status, DONE_STATUS);
        assert_eq!(facts.default_status, DEFAULT_STATUS);
        assert_eq!(facts.type_field, TYPE_FIELD);
        assert_eq!(
            facts.statuses,
            vec![DEFAULT_STATUS, MID_STATUS, DONE_STATUS]
        );

        let task_type = facts
            .task_types
            .iter()
            .find(|t| t.name == TASK_TYPE)
            .expect("the declared type");
        assert!(task_type.closed, "the type closes");
        let title = task_type
            .fields
            .iter()
            .find(|f| f.name == TITLE)
            .expect("title field");
        assert!(title.required, "title is required");

        let blocker = facts
            .relationships
            .iter()
            .find(|r| r.name == BLOCKER)
            .expect("blocker relationship");
        assert_eq!(blocker.kind, "blocker");
        assert_eq!(blocker.inverse, BLOCKER_INV);
    }

    #[test]
    fn facts_are_free_form_without_a_schema() {
        // The renamed schema-less store declares no task types, so the status is
        // free-form (under the renamed field name) and there are no type facts.
        let facts = prime(&store_renamed()).unwrap().facts;
        assert_eq!(facts.status_field, STATUS_FIELD);
        assert!(facts.statuses.is_empty(), "no declared status enum");
        assert!(facts.task_types.is_empty(), "no declared types");
    }

    #[test]
    fn summary_counts_open_as_total_minus_closed() {
        use crate::model::{MutationEvent, OpType};
        use serde_json::{Map, Value};

        // The schema-less renamed store keeps status values free (done = `closed`).
        let store = store_renamed();
        let with_status = |status: &str| {
            let mut m = Map::new();
            m.insert(STATUS_FIELD.to_string(), Value::from(status));
            m
        };
        store
            .append_events(&[
                MutationEvent::new(OpType::Create, "a", with_status("open")),
                MutationEvent::new(OpType::Create, "b", with_status("closed")),
            ])
            .unwrap();

        let summary = prime(&store).unwrap().facts.summary;
        assert_eq!(summary.total, 2);
        assert_eq!(summary.closed, 1);
        assert_eq!(summary.open, 1, "open = total - closed");
    }
}
