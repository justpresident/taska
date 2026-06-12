//! Schema law and the write gate: event vetting, `[task_types]` enforcement,
//! and schema-aware value shaping.
//!
//! This is **domain law, not frontend code** - every frontend (the bundled
//! CLI, a TUI, a library consumer) must funnel its writes through
//! [`vet_events`] or it can corrupt a schema'd store. Presentation stays out:
//! nothing here prints - reports come back as data
//! ([`schema_conformance_report`]) for the frontend to surface its own way.
//!
//! Everything here implements one law - **schemas are write-time law only**:
//! [`vet_events`] is the gate every draft batch passes through (no-op
//! dropping, reserved names, whole-task schema conformance), while the
//! read side stays tolerant ([`schema_conformance_report`] reports, never
//! errors, and [`substitute_schema_defaults`] papers over missing/invalid
//! values display-only). [`coerce_event_fields`] and [`dispatch_accumulate`]
//! shape values toward their declared kinds *before* the gate so it can stay
//! the single enforcer. [`FieldOps`] is the frontend-neutral description of
//! one write (a `set` map plus ordered `+=`/`-=` operand lists); how a frontend
//! produces it - the CLI's `key=value` grammar, a TUI form - is its own business.

use std::collections::{BTreeSet, HashMap};
use std::hash::BuildHasher;

use serde_json::{Map, Value};

use crate::config::Config;
use crate::error::DynError;
use crate::model::{
    MutationEvent, OpType, TaskState, REL_KEY, RESERVED_FIELD_KEYS, STATUS_KEY, TARGET_KEY,
    TASK_TYPE_KEY,
};

/// A parsed field list, split by operator - the frontend-neutral description
/// of one write.
///
/// The CLI builds it from `key=value` / `key+=value` / `key-=value` tokens
/// (`cli::parse_field_ops`); any other frontend can construct it directly.
pub struct FieldOps {
    /// Fields to **set** (`=`), values JSON-guessed. A map, so a repeated `key=`
    /// is last-wins (you're choosing the field's value).
    pub set: Map<String, Value>,
    /// `+=` operands, in token ORDER as `(field, value)` - a list, not a map, so
    /// repeated `field+=` on one field accumulate instead of overwriting
    /// (`tags+=a tags+=b` adds both). Dispatched by declared kind at write time:
    /// text append for strings/undeclared, `Add` for numeric and set fields.
    pub append: Vec<(String, Value)>,
    /// `-=` operands, in token order as `(field, value)` - like [`append`], a
    /// list so repeated `field-=` accumulate. Numeric subtract or set-element
    /// removal; requires a declared numeric/set field.
    ///
    /// [`append`]: FieldOps::append
    pub subtract: Vec<(String, Value)>,
    /// The verbatim inline token text per SET key (always `Value::String`).
    /// Schema-aware coercion uses it to recover exact input the JSON guess
    /// mangles - `version=3.10` guesses the number 3.1, but a declared string
    /// field wants "3.10". `@file`/`@-` values are already verbatim strings and
    /// have no entry. A `Map<String, Value>` (not strings) so the same
    /// [`canonicalize_fields`] keeps its keys aligned with `set`.
    pub raw: Map<String, Value>,
}

/// The display<->canonical field-name boundary, as `(display, canonical)` pairs.
///
/// Events and the baseline always store `status`/`task_type` under their
/// canonical keys ([`STATUS_KEY`]/[`TASK_TYPE_KEY`]); `[workflow] status_field`
/// /`type_field` are *display* names. This is the one shared list of that
/// mapping - the read pipeline renames canonical->display, the write side
/// ([`canonicalize_fields`]) renames display->canonical - so renaming either in
/// config is free, with no data migration.
pub const fn canonical_field_pairs(
    workflow: &crate::config::WorkflowConfig,
) -> [(&String, &'static str); 2] {
    [
        (&workflow.status_field, STATUS_KEY),
        (&workflow.type_field, TASK_TYPE_KEY),
    ]
}

/// Map a write payload's configured DISPLAY field names onto their canonical
/// storage keys, before vetting/appending.
///
/// The write-side inverse of the read pipeline's canonical->display rename, over
/// the shared [`canonical_field_pairs`] list so the two boundaries can never
/// disagree. Writing the canonical spelling directly while a different display
/// name is configured is rejected: one writable name per concept per store.
/// **Every frontend funnels its writes through this** (then [`vet_events`]) so
/// the log stays canonical regardless of the configured display names - the CLI
/// is one caller, not the owner.
pub fn canonicalize_fields(
    fields: &mut Map<String, Value>,
    workflow: &crate::config::WorkflowConfig,
) -> Result<(), DynError> {
    for (display, canonical) in canonical_field_pairs(workflow) {
        if display == canonical {
            continue;
        }
        if fields.contains_key(canonical) {
            return Err(format!(
                "`{canonical}` is the canonical storage key of the configured `{display}` \
                 field; set `{display}=` instead"
            )
            .into());
        }
        if let Some(value) = fields.remove(display.as_str()) {
            fields.insert(canonical.to_string(), value);
        }
    }
    Ok(())
}

/// [`canonicalize_fields`] for the ordered `(field, value)` operand lists.
///
/// Renames each pair's display key to its canonical storage key in place
/// (preserving order and repeats), with the same "don't write the canonical
/// spelling directly" rejection - so a renamed `state+=x` hits the same
/// single-valued-status rejection `status+=x` does. For
/// [`FieldOps::append`]/[`subtract`].
///
/// [`subtract`]: FieldOps::subtract
pub fn canonicalize_field_pairs(
    pairs: &mut [(String, Value)],
    workflow: &crate::config::WorkflowConfig,
) -> Result<(), DynError> {
    for (display, canonical) in canonical_field_pairs(workflow) {
        if display == canonical {
            continue;
        }
        if pairs.iter().any(|(k, _)| k == canonical) {
            return Err(format!(
                "`{canonical}` is the canonical storage key of the configured `{display}` \
                 field; set `{display}=` instead"
            )
            .into());
        }
        for (key, _) in pairs.iter_mut() {
            if key == display {
                *key = canonical.to_string();
            }
        }
    }
    Ok(())
}

/// The grandfathered-data report: every task whose RAW stored fields violate
/// its `[task_types]` schema, one line each.
///
/// A line reads `task `id`: first violation (+N more)`. Empty while schemas
/// are off. Shared by the CLI's one-line read warning and `ta config
/// validate`'s detailed listing - reads stay tolerant (this is a report,
/// never an error), while any WRITE to such a task must bring it into
/// conformance (the whole-task gate).
pub fn schema_conformance_report<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    config: &Config,
) -> Vec<String> {
    if config.task_types.types.is_empty() {
        return Vec::new();
    }
    let mut report: Vec<String> = state
        .values()
        .filter_map(|task| {
            // Under `untyped_tasks = "allow"`, a typeless task is sanctioned -
            // not reported anywhere. `warn` and `deny` both report it.
            if !task.custom_fields.contains_key(TASK_TYPE_KEY)
                && config.workflow.untyped_tasks == crate::config::UntypedTasks::Allow
            {
                return None;
            }
            let violations = schema_violations(&task.custom_fields, config);
            let first = violations.first()?;
            let more = match violations.len() {
                1 => String::new(),
                n => format!(" (+{} more)", n - 1),
            };
            Some(format!("task `{}`: {first}{more}", task.id))
        })
        .collect();
    report.sort();
    report
}

/// Validate a batch of draft events against the current `state` and drop the
/// redundant ones, returning exactly the events worth appending.
///
/// Meant to run inside the store's write lock (via `EventStore::append_checked`),
/// so the verify-then-write is atomic and can't race a concurrent writer. Rules
/// (a rejection is a hard error - nothing in the batch is written):
/// - Setting a reserved/computed field name (the envelope keys, `id`/`deps`/`dep`,
///   the timestamp and graph columns, relationship names) is **rejected**.
/// - `Create` of an existing id - or any op whose target task is absent (incl.
///   `Delete`) - is **rejected**, as is an `AddEdge` to itself or to a missing
///   target.
/// - An `Update` keeps only the fields that actually change (a value already
///   equal, or a `null`-unset of an already-absent field, is dropped); an
///   `Update` left with no fields is dropped entirely.
/// - `AddEdge` of an existing edge and `RemoveEdge` of an absent one are dropped as
///   no-ops.
/// - `Append` (`+=`) never lands on a no-op, but is rejected on the single-valued
///   status and task-type fields.
/// - Finally, [`enforce_schemas`] validates every touched task's RESULTING
///   field set against its `[task_types]` schema - whole-task, every
///   violation in one error.
pub fn vet_events<S: BuildHasher>(
    drafts: &[MutationEvent],
    state: &HashMap<String, TaskState, S>,
    config: &Config,
) -> Result<Vec<MutationEvent>, DynError> {
    let reserved = reserved_field_names(config);
    let mut out = Vec::new();
    // The would-be RESULTING fields of every task a surviving field-carrying
    // draft touches, simulated with the engine's own apply functions - the
    // schema gate validates WHOLE tasks (per the type-schemas decisions), not
    // just the drafts, so a write to a non-conforming task surfaces every
    // violation at once. `None` marks a task deleted within the batch.
    let mut preview: HashMap<String, Option<Map<String, Value>>> = HashMap::new();
    for draft in drafts {
        let id = draft.task_id.as_str();
        // A field whose value is computed/injected (id, deps, the timestamp and
        // graph columns, relationship names) can't be set directly - a user value
        // of the same name is silently shadowed. Applies to ops carrying fields.
        if matches!(
            draft.op,
            OpType::Create | OpType::Update | OpType::Append | OpType::Add | OpType::Remove
        ) {
            if let Some(bad) = draft.payload.keys().find(|k| reserved.contains(k.as_str())) {
                return Err(format!(
                    "`{bad}` is a reserved or computed field and can't be set directly"
                )
                .into());
            }
        }
        match draft.op {
            OpType::Create => {
                if state.contains_key(id) {
                    return Err(format!(
                        "task `{id}` already exists (use `ta update {id} ...` to change it)"
                    )
                    .into());
                }
                let mut fields = preview_entry(&mut preview, id, None);
                crate::engine::apply_set(&mut fields, draft.payload.clone());
                preview.insert(id.to_string(), Some(fields));
                out.push(draft.clone());
            }
            OpType::Update => {
                let task = require_existing(state, id)?;
                let mut payload = Map::new();
                for (key, value) in &draft.payload {
                    if changes_field(task, key, value) {
                        payload.insert(key.clone(), value.clone());
                    }
                }
                if !payload.is_empty() {
                    let mut fields = preview_entry(&mut preview, id, Some(task));
                    crate::engine::apply_set(&mut fields, payload.clone());
                    preview.insert(id.to_string(), Some(fields));
                    let mut event = draft.clone();
                    event.payload = payload;
                    out.push(event);
                }
            }
            OpType::Append => {
                require_existing(state, id)?;
                // Drafts are canonical by the time they reach the gate, so the
                // single-valued check looks for the storage keys (status and
                // the task-type discriminator), not the display names.
                if let Some(bad) = draft
                    .payload
                    .keys()
                    .find(|k| *k == STATUS_KEY || *k == TASK_TYPE_KEY)
                {
                    return Err(format!(
                        "can't append (`+=`) to `{bad}`: it holds a single value, not a log"
                    )
                    .into());
                }
                let task = state.get(id);
                let mut fields = preview_entry(&mut preview, id, task);
                crate::engine::apply_append(&mut fields, draft.payload.clone());
                preview.insert(id.to_string(), Some(fields));
                out.push(draft.clone()); // appends accumulate - never a no-op
            }
            OpType::Add | OpType::Remove => {
                let task = require_existing(state, id)?;
                if vet_accumulate(draft, task, &mut preview)? {
                    out.push(draft.clone());
                }
            }
            OpType::AddEdge => {
                let task = require_existing(state, id)?;
                let target = draft.payload.get(TARGET_KEY).and_then(Value::as_str);
                if target == Some(id) {
                    return Err(format!("a task can't reference itself (`{id}`)").into());
                }
                if let Some(t) = target {
                    if !state.contains_key(t) {
                        return Err(format!("no task `{t}` to reference").into());
                    }
                }
                if !dep_edge_exists(task, &draft.payload) {
                    out.push(draft.clone());
                }
            }
            OpType::RemoveEdge => {
                let task = require_existing(state, id)?;
                if dep_edge_exists(task, &draft.payload) {
                    out.push(draft.clone());
                }
            }
            OpType::Delete => {
                // Deleting a missing task is a typo, like any other mutation on it.
                require_existing(state, id)?;
                preview.insert(id.to_string(), None);
                out.push(draft.clone());
            }
        }
    }
    enforce_schemas(&preview, config)?;
    Ok(out)
}

/// The `Add`/`Remove` arm of [`vet_events`]: reject accumulating into a
/// single-valued field, apply onto the preview with the engine's own
/// semantics, and report whether anything changed - an accumulate that changes
/// nothing (inserting a present set element, removing an absent one, adding 0)
/// is dropped rather than logged.
fn vet_accumulate(
    draft: &MutationEvent,
    task: &TaskState,
    preview: &mut HashMap<String, Option<Map<String, Value>>>,
) -> Result<bool, DynError> {
    if let Some(bad) = draft
        .payload
        .keys()
        .find(|k| *k == STATUS_KEY || *k == TASK_TYPE_KEY)
    {
        return Err(
            format!("can't accumulate (`+=`/`-=`) into `{bad}`: it holds a single value").into(),
        );
    }
    let id = draft.task_id.as_str();
    let mut fields = preview_entry(preview, id, Some(task));
    let before = fields.clone();
    crate::engine::apply_accumulate(
        &mut fields,
        draft.payload.clone(),
        matches!(draft.op, OpType::Add),
    );
    let changed = fields != before;
    preview.insert(id.to_string(), Some(fields));
    Ok(changed)
}

/// The declared defaults a write should stamp.
///
/// Every field of the effective task type (the payload's discriminator wins
/// over the current one) that has a `default`, is absent from the current
/// task, and is not being set, unset, or accumulated by this very write.
/// Used by `create` (stamp into the payload) and `update` (heal the task on
/// any write), so a task with defaulted required fields conforms without
/// spelling them out.
pub fn schema_default_stamps(
    current: Option<&Map<String, Value>>,
    payload: &Map<String, Value>,
    touched: &BTreeSet<String>,
    config: &Config,
) -> Map<String, Value> {
    let mut stamps = Map::new();
    let Some(def) = payload
        .get(TASK_TYPE_KEY)
        .or_else(|| current.and_then(|fields| fields.get(TASK_TYPE_KEY)))
        .and_then(Value::as_str)
        .and_then(|name| config.task_types.types.get(name))
    else {
        return stamps;
    };
    for (name, schema) in &def.fields {
        let Some(default) = schema.default_value() else {
            continue;
        };
        let key = declared_field_key(name, &config.workflow.status_field);
        let absent = !payload.contains_key(key)
            && !touched.contains(key)
            && !current.is_some_and(|fields| fields.contains_key(key));
        if absent {
            stamps.insert(key.to_string(), default.clone());
        }
    }
    stamps
}

/// Read-side default substitution (display-only, like the timestamp
/// injection).
///
/// A declared field that is MISSING or whose stored value is invalid (wrong
/// kind or constraint-violating) reads as its declared `default`. The stored
/// log/baseline are untouched - the non-conformance report and `ta repair
/// --schema` remain the signals to actually fix the data. Runs on RAW state,
/// before the display renames.
pub fn substitute_schema_defaults<S: BuildHasher>(
    state: &mut HashMap<String, TaskState, S>,
    config: &Config,
) {
    if config.task_types.types.is_empty() {
        return;
    }
    for task in state.values_mut() {
        let Some(def) = task
            .custom_fields
            .get(TASK_TYPE_KEY)
            .and_then(Value::as_str)
            .and_then(|name| config.task_types.types.get(name))
        else {
            continue;
        };
        let mut substitutions: Vec<(String, Value)> = Vec::new();
        for (name, schema) in &def.fields {
            let Some(default) = schema.default_value() else {
                continue;
            };
            let Ok(kind) = crate::config::FieldKind::parse(schema.kind_str()) else {
                continue;
            };
            let key = declared_field_key(name, &config.workflow.status_field);
            let invalid = task.custom_fields.get(key).is_none_or(|value| {
                !kind.matches_value(value, schema.values())
                    || !schema.constraint_violations(value).is_empty()
            });
            if invalid {
                substitutions.push((key.to_string(), default.clone()));
            }
        }
        for (key, value) in substitutions {
            task.custom_fields.insert(key, value);
        }
    }
}

/// Take a task's working field set out of the preview (falling back to its
/// current state, then to empty for a fresh create), for [`vet_events`] to
/// apply the next draft onto.
fn preview_entry(
    preview: &mut HashMap<String, Option<Map<String, Value>>>,
    id: &str,
    base: Option<&TaskState>,
) -> Map<String, Value> {
    preview
        .remove(id)
        .flatten()
        .or_else(|| base.map(|t| t.custom_fields.clone()))
        .unwrap_or_default()
}

/// The schema gate tail of [`vet_events`]: whole-task conformance for every
/// touched (and surviving) previewed task, with EVERY violation in one error so
/// a user or LLM can fix them all in a single follow-up. Inert while
/// `[task_types]` declares nothing (the schema-agnostic floor).
fn enforce_schemas(
    preview: &HashMap<String, Option<Map<String, Value>>>,
    config: &Config,
) -> Result<(), DynError> {
    if config.task_types.types.is_empty() {
        return Ok(());
    }
    for (id, fields) in preview {
        let Some(fields) = fields else { continue };
        // The untyped-tasks policy: under `allow`/`warn`, a task with NO type
        // is outside the schemas - writes proceed unvalidated (the migration
        // ladder's lax rungs). Only `deny` makes the type mandatory here.
        if !fields.contains_key(TASK_TYPE_KEY)
            && config.workflow.untyped_tasks != crate::config::UntypedTasks::Deny
        {
            continue;
        }
        let violations = schema_violations(fields, config);
        if !violations.is_empty() {
            return Err(format!(
                "task `{id}` does not conform to its task-type schema:\n  - {}",
                violations.join("\n  - ")
            )
            .into());
        }
    }
    Ok(())
}

/// The stored key a DECLARED schema field name refers to.
///
/// Declarations use display names, storage is canonical - only the status
/// field differs (the discriminator can't be declared; `Config::validate`
/// enforces both rules).
pub fn declared_field_key<'a>(name: &'a str, status_display: &str) -> &'a str {
    if name == status_display {
        STATUS_KEY
    } else {
        name
    }
}

/// Every way `fields` (a task's would-be stored fields) violates the declared
/// `[task_types]` schemas: a missing/non-string/unknown discriminator, a missing
/// required field, a value not matching its declared kind, or an undeclared
/// field on a `closed` type. Field names in declarations are DISPLAY names
/// (the status field may be declared under its configured name); stored keys
/// are canonical, so names resolve through the same pairs the write/read
/// boundaries use. Empty = conforming.
fn schema_violations(fields: &Map<String, Value>, config: &Config) -> Vec<String> {
    let w = &config.workflow;
    let types = &config.task_types.types;
    let declared_types = || types.keys().cloned().collect::<Vec<_>>().join(", ");
    let mut violations = Vec::new();

    // Resolve the task's type by the canonical key (drafts are canonical here).
    let Some(type_value) = fields.get(TASK_TYPE_KEY) else {
        violations.push(format!(
            "missing the `{}` field (declared task types: {})",
            w.type_field,
            declared_types()
        ));
        return violations;
    };
    let Some(type_name) = type_value.as_str() else {
        violations.push(format!(
            "`{}` must be a string naming a task type (one of: {})",
            w.type_field,
            declared_types()
        ));
        return violations;
    };
    let Some(def) = types.get(type_name) else {
        violations.push(format!(
            "unknown task type `{type_name}` (declared: {})",
            declared_types()
        ));
        return violations;
    };

    for (name, schema) in &def.fields {
        // validate() guarantees the kind parses; stay defensive anyway.
        let Ok(kind) = crate::config::FieldKind::parse(schema.kind_str()) else {
            continue;
        };
        match fields.get(declared_field_key(name, &w.status_field)) {
            None => {
                if schema.required() {
                    let hint = if schema.values().is_empty() {
                        String::new()
                    } else {
                        format!(" (one of: {})", schema.values().join(", "))
                    };
                    violations.push(format!("missing required field `{name}`{hint}"));
                }
            }
            Some(value) => {
                if kind.matches_value(value, schema.values()) {
                    // Kind-correct values still face the declared constraints
                    // (min/max, pattern, length and item bounds).
                    for constraint in schema.constraint_violations(value) {
                        violations.push(format!("`{name}`: {value} {constraint}"));
                    }
                } else {
                    let hint = if schema.values().is_empty() {
                        String::new()
                    } else {
                        format!(" (one of: {})", schema.values().join(", "))
                    };
                    violations.push(format!(
                        "`{name}`: expected {}{hint}, got {value}",
                        schema.kind_str()
                    ));
                }
            }
        }
    }

    if def.closed {
        let allowed: BTreeSet<&str> = def
            .fields
            .keys()
            .map(|name| declared_field_key(name, &w.status_field))
            .chain([TASK_TYPE_KEY, STATUS_KEY])
            .collect();
        for key in fields.keys() {
            if !allowed.contains(key.as_str()) {
                violations.push(format!(
                    "undeclared field `{key}` (task type `{type_name}` is closed; declared \
                     fields: {})",
                    def.fields.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    violations
}

/// Schema-aware value coercion for `Create`/`Update` payloads, run under the
/// store lock (where the task's type is known) just before [`vet_events`].
///
/// Best-effort lifting toward each DECLARED field's kind - the write gate
/// stays the enforcer with its messages:
/// - declared string: a guessed scalar reverts to its verbatim token (`raw`)
///   or stringifies, so `version=3.10` stores `"3.10"`, not the number 3.1;
/// - declared int/uint/float: a (quoted) numeric string parses to a number;
/// - declared bool: the strings "true"/"false" parse;
/// - declared array<T>/set<T>: a bare scalar lifts to a singleton, elements
///   coerce per T, and a set canonicalizes to its STORED form - deduped and
///   sorted (`cmp_json` order) - so concurrent inserts converge bytewise and
///   re-adding an element is a no-op, like relationship edges.
///
/// Undeclared fields, unknown/missing types, and `Append` payloads keep the
/// JSON-or-string guess (the schema-agnostic floor; `+=` operands are shaped
/// by [`dispatch_accumulate`] instead). The payload's own (re)typed
/// discriminator wins over the task's current type, so retype + fields coerce
/// against the new schema.
pub fn coerce_event_fields<S: BuildHasher>(
    events: &mut [MutationEvent],
    raw: &Map<String, Value>,
    state: &HashMap<String, TaskState, S>,
    config: &Config,
) {
    if config.task_types.types.is_empty() {
        return;
    }
    for event in events.iter_mut() {
        if !matches!(event.op, OpType::Create | OpType::Update) {
            continue;
        }
        let type_name = event
            .payload
            .get(TASK_TYPE_KEY)
            .or_else(|| {
                state
                    .get(&event.task_id)
                    .and_then(|t| t.custom_fields.get(TASK_TYPE_KEY))
            })
            .and_then(Value::as_str);
        let Some(def) = type_name.and_then(|n| config.task_types.types.get(n)) else {
            continue;
        };
        for (name, schema) in &def.fields {
            let key = declared_field_key(name, &config.workflow.status_field);
            let Some(value) = event.payload.get(key) else {
                continue;
            };
            if value.is_null() {
                continue; // the unset convention is never coerced
            }
            let Ok(kind) = crate::config::FieldKind::parse(schema.kind_str()) else {
                continue; // validate() reports the bad declaration
            };
            if let Some(coerced) = coerce_value(value, &kind, raw.get(key)) {
                event.payload.insert(key.to_string(), coerced);
            }
        }
    }
}

/// One value's lift toward `kind` (see [`coerce_event_fields`]); `None` = leave
/// it for the gate to judge as-is.
pub fn coerce_value(
    value: &Value,
    kind: &crate::config::FieldKind,
    raw: Option<&Value>,
) -> Option<Value> {
    use crate::config::FieldKind as K;
    match kind {
        K::String => match value {
            Value::Number(_) | Value::Bool(_) => Some(
                raw.cloned()
                    .unwrap_or_else(|| Value::String(value.to_string())),
            ),
            _ => None,
        },
        K::Int | K::Uint | K::Float => value
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
            .filter(Value::is_number),
        K::Bool => match value.as_str() {
            Some("true") => Some(Value::Bool(true)),
            Some("false") => Some(Value::Bool(false)),
            _ => None,
        },
        K::Datetime | K::Enum | K::Any => None,
        K::Array(element) => Some(coerce_sequence(value, element, raw, false)),
        K::Set(element) => Some(coerce_sequence(value, element, raw, true)),
    }
}

/// Coerce toward `array<element>`/`set<element>`: lift a bare scalar to a
/// singleton (its `raw` token still applies), coerce each element, and give a
/// set its canonical stored form - sorted (`cmp_json`) and deduped (by compact
/// JSON) - the bytewise form concurrent writers converge on.
fn coerce_sequence(
    value: &Value,
    element: &crate::config::FieldKind,
    raw: Option<&Value>,
    canonical_set: bool,
) -> Value {
    let mut items: Vec<Value> = match value {
        Value::Array(items) => items
            .iter()
            .map(|item| coerce_value(item, element, None).unwrap_or_else(|| item.clone()))
            .collect(),
        scalar => vec![coerce_value(scalar, element, raw).unwrap_or_else(|| scalar.clone())],
    };
    if canonical_set {
        items.sort_by(crate::model::cmp_json);
        items.dedup_by(|a, b| a == b);
    }
    Value::Array(items)
}

/// Build the events for one `update`'s field operations, run under the store
/// lock (the accumulate dispatch needs the task's type from live state).
///
/// The `Update` (set) event comes first so `field=reset field+=more` applies
/// the reset before accumulating, independent of token order; the
/// schema-aware coercion then shapes the set values.
pub fn build_field_events<S: BuildHasher>(
    id: &str,
    ops: &FieldOps,
    state: &HashMap<String, TaskState, S>,
    config: &Config,
) -> Result<Vec<MutationEvent>, DynError> {
    let (text, add, remove) = dispatch_accumulate(id, ops, state, config)?;
    // Heal-on-write: any write to a task whose declared, DEFAULTED fields are
    // still absent stamps them in the same Update - so `required` + `default`
    // never blocks a write, and the task converges toward conformance.
    let mut set = ops.set.clone();
    let touched: BTreeSet<String> = text
        .keys()
        .chain(add.keys())
        .chain(remove.keys())
        .cloned()
        .collect();
    let current = state.get(id).map(|task| &task.custom_fields);
    for (key, value) in schema_default_stamps(current, &set, &touched, config) {
        set.insert(key, value);
    }
    let mut events = Vec::new();
    for (payload, op) in [
        (set, OpType::Update),
        (text, OpType::Append),
        (add, OpType::Add),
        (remove, OpType::Remove),
    ] {
        if !payload.is_empty() {
            events.push(MutationEvent::new(op, id, payload));
        }
    }
    coerce_event_fields(&mut events, &ops.raw, state, config);
    Ok(events)
}

/// [`dispatch_accumulate`]'s result: the `(Append, Add, Remove)` payloads.
type AccumulatePayloads = (Map<String, Value>, Map<String, Value>, Map<String, Value>);

/// Split the `+=`/`-=` maps into per-op payloads by each field's DECLARED kind.
/// The keyboard vocabulary stays `{=, +=, -=}` while the event vocabulary
/// dispatches:
/// - `+=`: strings, `any`, undeclared fields, and unknown/missing types keep
///   the text `Append` (the schema-agnostic floor); int/uint/float and
///   `set<T>` become `Add` (set operands lift to element arrays, so replay's
///   set path is unambiguous); bool/enum/datetime/`array<T>` reject.
/// - `-=`: int/uint/float and `set<T>` become `Remove`; anything else rejects
///   (`-=` has no meaning without subtraction or removal semantics).
///
/// The applicable schema: the set payload's (re)typed discriminator wins over
/// the task's current type, mirroring [`coerce_event_fields`].
///
/// `pub(crate)` so `create` can fold the SAME accumulation into a new task's
/// initial value (its field starts absent, so the combined operands are the
/// value), keeping `+=` consistent between create and update.
pub(crate) fn dispatch_accumulate<S: BuildHasher>(
    id: &str,
    ops: &FieldOps,
    state: &HashMap<String, TaskState, S>,
    config: &Config,
) -> Result<AccumulatePayloads, DynError> {
    use crate::config::FieldKind;
    let type_name = ops
        .set
        .get(TASK_TYPE_KEY)
        .or_else(|| {
            state
                .get(id)
                .and_then(|t| t.custom_fields.get(TASK_TYPE_KEY))
        })
        .and_then(Value::as_str);
    let def = type_name.and_then(|n| config.task_types.types.get(n));
    let declared_kind = |field: &str| -> Option<(&str, FieldKind)> {
        def?.fields.iter().find_map(|(name, schema)| {
            (declared_field_key(name, &config.workflow.status_field) == field)
                .then(|| FieldKind::parse(schema.kind_str()).ok())
                .flatten()
                .map(|kind| (schema.kind_str(), kind))
        })
    };

    // Repeated `field+=`/`field-=` for one field accumulate (the operands arrive
    // in token order): numbers sum, set elements gather into one operand, text
    // joins with `\n` - each combined the SAME way replay would, so the result is
    // one event per field carrying the whole accumulation.
    let (mut text, mut add, mut remove) = (Map::new(), Map::new(), Map::new());
    for (key, operand) in &ops.append {
        match declared_kind(key) {
            Some((_, kind @ (FieldKind::Int | FieldKind::Uint | FieldKind::Float))) => {
                let operand = coerce_value(operand, &kind, None).unwrap_or_else(|| operand.clone());
                accumulate_number(&mut add, key, &operand);
            }
            Some((_, FieldKind::Set(element))) => {
                extend_array(
                    &mut add,
                    key,
                    coerce_sequence(operand, &element, None, true),
                );
            }
            Some((kind_str, FieldKind::Bool | FieldKind::Enum | FieldKind::Datetime)) => {
                return Err(format!(
                    "`+=` is not defined for `{key}` (declared {kind_str}); set it with `{key}=`"
                )
                .into());
            }
            Some((kind_str, FieldKind::Array(_))) => {
                return Err(format!(
                    "`+=` is not defined for `{key}` (declared {kind_str}, which allows \
                     duplicates and keeps order); set the whole value with `{key}=[...]`, or \
                     declare it set<...> for element inserts"
                )
                .into());
            }
            // Strings, `any`, undeclared fields, unknown/missing type: text.
            _ => {
                append_into_text(&mut text, key, operand);
            }
        }
    }
    for (key, operand) in &ops.subtract {
        match declared_kind(key) {
            Some((_, kind @ (FieldKind::Int | FieldKind::Uint | FieldKind::Float))) => {
                let operand = coerce_value(operand, &kind, None).unwrap_or_else(|| operand.clone());
                accumulate_number(&mut remove, key, &operand);
            }
            Some((_, FieldKind::Set(element))) => {
                extend_array(
                    &mut remove,
                    key,
                    coerce_sequence(operand, &element, None, true),
                );
            }
            _ => {
                return Err(format!(
                    "`-=` needs a field declared as a number or set<...> (`{key}` isn't)"
                )
                .into());
            }
        }
    }
    Ok((text, add, remove))
}

/// Fold a numeric `+=`/`-=` operand into a per-field accumulator: repeated
/// operands on one field SUM, so the single `Add`/`Remove` event applies the
/// total once (`points+=2 points+=3` accumulates to 5). Summed via the engine's
/// own [`crate::engine::accumulate_numbers`], so build-time and replay math
/// agree.
fn accumulate_number(map: &mut Map<String, Value>, key: &str, operand: &Value) {
    let combined = match (map.get(key), operand) {
        (Some(prev), Value::Number(n)) => crate::engine::accumulate_numbers(Some(prev), n, true)
            .unwrap_or_else(|| operand.clone()),
        _ => operand.clone(),
    };
    map.insert(key.to_string(), combined);
}

/// Concatenate a coerced set-element array into a per-field accumulator: repeated
/// `field+=`/`field-=` on one set field gather all elements into one operand
/// (replay's set insert/remove dedups), so `tags+=a tags+=b` carries both.
fn extend_array(map: &mut Map<String, Value>, key: &str, operand: Value) {
    match (map.get_mut(key), operand) {
        (Some(Value::Array(existing)), Value::Array(more)) => existing.extend(more),
        (_, operand) => {
            map.insert(key.to_string(), operand);
        }
    }
}

/// Join a text `+=` operand into a per-field accumulator: a single operand is
/// stored as-is, repeated ones join with `\n` - the same separator replay's
/// `Append` uses - so `notes+=a notes+=b` becomes one `Append` of "a\nb".
fn append_into_text(map: &mut Map<String, Value>, key: &str, operand: &Value) {
    use crate::engine::append_text;
    match map.get(key) {
        Some(prev) => {
            let joined = format!("{}\n{}", append_text(prev), append_text(operand));
            map.insert(key.to_string(), Value::String(joined));
        }
        None => {
            map.insert(key.to_string(), operand.clone());
        }
    }
}

/// The full set of field names the write gate refuses: the static
/// [`RESERVED_FIELD_KEYS`] plus the config-dependent computed/injected names -
/// the configured timestamp columns and the relationship type names + inverses
/// (which `show` surfaces and `ta dep` edits). A user field with any of these
/// names would be silently shadowed at read time, so meaningless and invisible.
fn reserved_field_names(config: &Config) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = RESERVED_FIELD_KEYS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for name in [
        &config.timestamps.create_time,
        &config.timestamps.update_time,
        &config.timestamps.close_time,
    ] {
        if !name.is_empty() {
            names.insert(name.clone());
        }
    }
    for (name, def) in &config.relationships.types {
        names.insert(name.clone());
        if !def.inverse.is_empty() {
            names.insert(def.inverse.clone());
        }
    }
    names
}

/// The task `id` in `state`, or an error if it doesn't exist - so a mutation
/// against a typo'd/absent task is rejected at write time rather than becoming a
/// silent orphan. (Replay still tolerates orphans from merges/reverts.)
fn require_existing<'a, S: BuildHasher>(
    state: &'a HashMap<String, TaskState, S>,
    id: &str,
) -> Result<&'a TaskState, DynError> {
    state
        .get(id)
        .ok_or_else(|| format!("no task `{id}`").into())
}

/// Whether setting `key` = `value` would change `task`. A `null` value is the
/// unset convention: it changes only a field that is currently present.
fn changes_field(task: &TaskState, key: &str, value: &Value) -> bool {
    if value.is_null() {
        task.custom_fields.contains_key(key)
    } else {
        task.custom_fields.get(key) != Some(value)
    }
}

/// Whether `task` already has the edge described by an `AddEdge`/`RemoveEdge`
/// payload (its `target` + `rel`). A payload missing either is malformed, so it
/// can't be a no-op.
fn dep_edge_exists(task: &TaskState, payload: &Map<String, Value>) -> bool {
    let (Some(target), Some(rel_type)) = (
        payload.get(TARGET_KEY).and_then(Value::as_str),
        payload.get(REL_KEY).and_then(Value::as_str),
    ) else {
        return false;
    };
    task.relationships
        .get(rel_type)
        .is_some_and(|targets| targets.iter().any(|d| d == target))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;

    #[test]
    fn canonicalize_maps_display_status_and_rejects_the_canonical_spelling() {
        use crate::config::WorkflowConfig;
        let renamed = WorkflowConfig {
            status_field: "state".to_string(),
            ..WorkflowConfig::default()
        };

        // The configured display name maps onto the canonical storage key.
        let mut fields = Map::new();
        fields.insert("state".to_string(), serde_json::json!("open"));
        canonicalize_fields(&mut fields, &renamed).unwrap();
        assert_eq!(fields.get(STATUS_KEY), Some(&serde_json::json!("open")));
        assert!(!fields.contains_key("state"), "display key consumed");

        // Writing the canonical spelling directly is rejected while a different
        // display name is configured - one writable name per concept.
        let mut direct = Map::new();
        direct.insert(STATUS_KEY.to_string(), serde_json::json!("x"));
        let err = canonicalize_fields(&mut direct, &renamed).unwrap_err();
        assert!(
            err.to_string().contains("state"),
            "points at display: {err}"
        );

        // Default name: canonical IS the display name; nothing to do.
        let mut plain = Map::new();
        plain.insert(STATUS_KEY.to_string(), serde_json::json!("open"));
        canonicalize_fields(&mut plain, &WorkflowConfig::default()).unwrap();
        assert_eq!(plain.get(STATUS_KEY), Some(&serde_json::json!("open")));
    }

    #[test]
    fn schema_gate_validates_whole_tasks_and_lists_every_violation() {
        let config: Config = toml::from_str(
            r#"
[task_types.bug]
closed = true
[task_types.bug.fields]
points = "uint"
tags = "set<string>"
[task_types.bug.fields.severity]
type = "enum"
values = ["low", "high"]
required = true
[task_types.feature.fields.owner]
type = "string"
required = true
"#,
        )
        .unwrap();
        let create = |id: &str, fields: &[(&str, Value)]| {
            let payload: Map<String, Value> = fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect();
            MutationEvent::new(OpType::Create, id, payload)
        };
        let empty = HashMap::new();

        // Missing discriminator: rejected naming the display field and options.
        let err = vet_events(&[create("t", &[])], &empty, &config).unwrap_err();
        assert!(
            err.to_string().contains("missing the `type` field")
                && err.to_string().contains("bug, feature"),
            "{err}"
        );

        // EVERY violation in one error: missing required + wrong kind + closed.
        let err = vet_events(
            &[create(
                "t",
                &[
                    ("task_type", serde_json::json!("bug")),
                    ("points", serde_json::json!("abc")),
                    ("extra", serde_json::json!(1)),
                ],
            )],
            &empty,
            &config,
        )
        .unwrap_err()
        .to_string();
        for needle in [
            "missing required field `severity` (one of: low, high)",
            "`points`: expected uint",
            "undeclared field `extra`",
        ] {
            assert!(err.contains(needle), "`{needle}` in: {err}");
        }

        // A conforming create passes; set<string> rejects duplicates.
        let ok = create(
            "t",
            &[
                ("task_type", serde_json::json!("bug")),
                ("severity", serde_json::json!("low")),
                ("tags", serde_json::json!(["a", "b"])),
            ],
        );
        assert!(vet_events(&[ok], &empty, &config).is_ok());
        let dup = create(
            "t",
            &[
                ("task_type", serde_json::json!("bug")),
                ("severity", serde_json::json!("low")),
                ("tags", serde_json::json!(["a", "a"])),
            ],
        );
        let err = vet_events(&[dup], &empty, &config).unwrap_err();
        assert!(err.to_string().contains("expected set<string>"), "{err}");

        // No [task_types] declared: the gate is inert (schema-agnostic floor).
        assert!(vet_events(&[create("t", &[])], &empty, &Config::default()).is_ok());
    }

    #[test]
    fn schema_coercion_lifts_declared_values() {
        let config: Config = toml::from_str(
            r#"
[task_types.bug.fields]
version = "string"
points = "uint"
flag = "bool"
tags = "set<string>"
nums = "array<int>"
"#,
        )
        .unwrap();
        let raw: Map<String, Value> =
            std::iter::once(("version".to_string(), serde_json::json!("3.10"))).collect();
        let payload: Map<String, Value> = [
            ("task_type", serde_json::json!("bug")),
            ("version", serde_json::json!(3.1)),
            ("points", serde_json::json!("5")),
            ("flag", serde_json::json!("true")),
            ("tags", serde_json::json!(["b", "a", "b"])),
            ("nums", serde_json::json!(7)),
            ("free", serde_json::json!("kept")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut events = vec![MutationEvent::new(OpType::Create, "t", payload)];
        coerce_event_fields(&mut events, &raw, &HashMap::new(), &config);
        let p = &events[0].payload;
        assert_eq!(
            p["version"],
            serde_json::json!("3.10"),
            "raw token wins for a declared string (the guess said 3.1)"
        );
        assert_eq!(p["points"], serde_json::json!(5), "numeric string parses");
        assert_eq!(p["flag"], serde_json::json!(true));
        assert_eq!(
            p["tags"],
            serde_json::json!(["a", "b"]),
            "set canonical form: sorted + deduped"
        );
        assert_eq!(
            p["nums"],
            serde_json::json!([7]),
            "bare scalar lifts to a singleton"
        );
        assert_eq!(
            p["free"],
            serde_json::json!("kept"),
            "undeclared fields keep the guess"
        );
        // The coerced create then passes the gate.
        assert!(vet_events(&events, &HashMap::new(), &config).is_ok());

        // Without [task_types] nothing is touched (the schema-agnostic floor).
        let before = events[0].payload.clone();
        let mut untouched = events.clone();
        coerce_event_fields(&mut untouched, &raw, &HashMap::new(), &Config::default());
        assert_eq!(untouched[0].payload, before);
    }

    #[test]
    fn accumulate_dispatch_follows_declared_kinds() {
        use crate::test_support::{state, task};
        let config: Config = toml::from_str(
            r#"
[task_types.bug.fields]
points = "uint"
tags = "set<string>"
notes = "string"
flag = "bool"
"#,
        )
        .unwrap();
        let existing = state(&[task(
            "t",
            &[],
            &[
                ("task_type", serde_json::json!("bug")),
                ("points", serde_json::json!(3)),
                ("tags", serde_json::json!(["a"])),
            ],
        )]);
        let ops = |append: &[(&str, Value)], subtract: &[(&str, Value)]| FieldOps {
            set: Map::new(),
            append: append
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            subtract: subtract
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            raw: Map::new(),
        };

        // `+=` dispatch: numeric -> Add (string operand parses), set -> Add
        // (scalar lifts to an element array), string/undeclared -> Append.
        let events = build_field_events(
            "t",
            &ops(
                &[
                    ("points", serde_json::json!("2")),
                    ("tags", serde_json::json!("b")),
                    ("notes", serde_json::json!("x")),
                    ("free", serde_json::json!("y")),
                ],
                &[("points", serde_json::json!(1))],
            ),
            &existing,
            &config,
        )
        .unwrap();
        let by_op = |op: OpType| {
            events
                .iter()
                .find(|e| e.op == op)
                .map(|e| e.payload.clone())
                .unwrap_or_default()
        };
        let add = by_op(OpType::Add);
        assert_eq!(add["points"], serde_json::json!(2), "operand parsed");
        assert_eq!(add["tags"], serde_json::json!(["b"]), "scalar lifted");
        let append = by_op(OpType::Append);
        assert!(
            append.contains_key("notes") && append.contains_key("free"),
            "strings and undeclared stay text appends"
        );
        assert_eq!(by_op(OpType::Remove)["points"], serde_json::json!(1));

        // Rejections: `+=` on bool, `-=` without a numeric/set declaration.
        let err = build_field_events(
            "t",
            &ops(&[("flag", serde_json::json!("true"))], &[]),
            &existing,
            &config,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not defined"), "{err}");
        let err = build_field_events(
            "t",
            &ops(&[], &[("free", serde_json::json!(1))]),
            &existing,
            &config,
        )
        .unwrap_err();
        assert!(err.to_string().contains("declared as a number"), "{err}");

        // No schema at all: `+=` is plain text append (the floor).
        let events = build_field_events(
            "t",
            &ops(&[("points", serde_json::json!(2))], &[]),
            &existing,
            &Config::default(),
        )
        .unwrap();
        assert_eq!(events[0].op, OpType::Append, "floor keeps text append");
    }

    #[test]
    fn accumulate_no_ops_drop_and_results_validate() {
        use crate::test_support::{state, task};
        let config: Config =
            toml::from_str("[task_types.bug.fields]\npoints = \"uint\"\ntags = \"set<string>\"\n")
                .unwrap();
        let existing = state(&[task(
            "t",
            &[],
            &[
                ("task_type", serde_json::json!("bug")),
                ("points", serde_json::json!(3)),
                ("tags", serde_json::json!(["a"])),
            ],
        )]);
        let ops = |append: &[(&str, Value)], subtract: &[(&str, Value)]| FieldOps {
            set: Map::new(),
            append: append
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            subtract: subtract
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            raw: Map::new(),
        };

        // No-op accumulates are dropped by the gate: inserting a present set
        // element and adding 0 write nothing.
        let noop = build_field_events(
            "t",
            &ops(
                &[
                    ("tags", serde_json::json!("a")),
                    ("points", serde_json::json!(0)),
                ],
                &[],
            ),
            &existing,
            &config,
        )
        .unwrap();
        assert!(
            vet_events(&noop, &existing, &config).unwrap().is_empty(),
            "no-op accumulates never reach the log"
        );

        // A uint underflow is rejected by whole-task validation of the
        // previewed RESULT.
        let underflow = build_field_events(
            "t",
            &ops(&[], &[("points", serde_json::json!(5))]),
            &existing,
            &config,
        )
        .unwrap();
        let err = vet_events(&underflow, &existing, &config).unwrap_err();
        assert!(
            err.to_string().contains("expected uint"),
            "underflow caught by the result check: {err}"
        );
    }

    #[test]
    fn schema_gate_revalidates_on_retype() {
        use crate::test_support::{state, task};
        let config: Config = toml::from_str(
            r#"
[task_types.bug.fields.severity]
type = "enum"
values = ["low", "high"]
required = true
[task_types.feature.fields.owner]
type = "string"
required = true
"#,
        )
        .unwrap();
        // Whole-task on update: retyping revalidates against the NEW type, and
        // one update can fix everything at once.
        let existing = state(&[task(
            "t",
            &[],
            &[
                ("task_type", serde_json::json!("bug")),
                ("severity", serde_json::json!("low")),
            ],
        )]);
        let retype = MutationEvent::new(
            OpType::Update,
            "t",
            std::iter::once(("task_type".to_string(), serde_json::json!("feature"))).collect(),
        );
        let err = vet_events(&[retype], &existing, &config).unwrap_err();
        assert!(
            err.to_string().contains("missing required field `owner`"),
            "{err}"
        );
        let retype_fixed = MutationEvent::new(
            OpType::Update,
            "t",
            [
                ("task_type".to_string(), serde_json::json!("feature")),
                ("owner".to_string(), serde_json::json!("bob")),
            ]
            .into_iter()
            .collect(),
        );
        assert!(vet_events(&[retype_fixed], &existing, &config).is_ok());
    }

    #[test]
    fn schema_gate_resolves_renamed_status_display_name() {
        use crate::test_support::{state, task};
        // The schema declares the status under its DISPLAY name `state`; the
        // stored key is canonical `status` - the gate must match them up.
        let config: Config = toml::from_str(
            r#"
[workflow]
status_field = "state"
[task_types.job.fields.state]
type = "enum"
values = ["todo", "done"]
required = true
"#,
        )
        .unwrap();
        let existing = state(&[task(
            "j",
            &[],
            &[
                ("task_type", serde_json::json!("job")),
                ("status", serde_json::json!("todo")), // canonical storage
            ],
        )]);
        let touch = MutationEvent::new(
            OpType::Update,
            "j",
            std::iter::once(("note".to_string(), serde_json::json!("x"))).collect(),
        );
        assert!(
            vet_events(&[touch], &existing, &config).is_ok(),
            "declared display name matches canonical storage"
        );
        // And a bad stored status is reported under the DECLARED name.
        let bad = MutationEvent::new(
            OpType::Update,
            "j",
            std::iter::once(("status".to_string(), serde_json::json!("nope"))).collect(),
        );
        let err = vet_events(&[bad], &existing, &config).unwrap_err();
        assert!(err.to_string().contains("`state`: expected enum"), "{err}");
    }
}
