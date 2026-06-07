//! `ta repair` — bring a store's on-disk format (`--migrate`) or its DATA
//! (`--schema`, `--rename`) up to scratch.
//!
//! The schema/rename fixes rewrite log and baseline entries IN PLACE without a
//! confirmation prompt — repair is the one sanctioned non-append-only writer
//! (like `--migrate`), and the review surface is git-native: `git diff` shows
//! exactly what changed, `git restore` of the `.taska` paths reverts it before
//! a commit. Fixes are deterministic functions of (store, config), so two
//! clones that run the same repair converge bytewise. Anything ambiguous is
//! LISTED with a suggested command, never guessed.

use serde_json::Value;

use crate::config::{Config, FieldKind};
use crate::engine::Engine;
use crate::error::DynError;
use crate::migrate::{run_all, Snapshot};
use crate::model::{MutationEvent, OpType, TaskState, RESERVED_FIELD_KEYS, TASK_TYPE_KEY};
use crate::schema::schema_conformance_report;
use crate::storage::EventStore;

pub fn cmd_repair(
    store: &impl EventStore,
    migrate: bool,
    schema: bool,
    rename: Option<&str>,
    set_type_if_none: Option<&str>,
) -> Result<(), DynError> {
    if !migrate && !schema && rename.is_none() && set_type_if_none.is_none() {
        println!(
            "Nothing to do. Pass `--migrate` (on-disk format), `--schema` (lossless fixes \
             toward the declared task types), `--rename new=old`, and/or \
             `--set-type-if-none TYPE`."
        );
        return Ok(());
    }
    if migrate {
        repair_migrate(store)?;
    }
    if schema || rename.is_some() || set_type_if_none.is_some() {
        repair_schema(store, rename, set_type_if_none)?;
    }
    Ok(())
}

/// `--migrate`: run the stacking format migrations (see `crate::migrate`).
fn repair_migrate(store: &impl EventStore) -> Result<(), DynError> {
    let mut snap = Snapshot {
        log: store.load_mutations()?,
        baseline: store.load_baseline()?,
    };
    let report = run_all(&mut snap, store.config());
    if report.is_empty() {
        println!("Already up to date; nothing to migrate.");
        return Ok(());
    }
    // Rewrite both files in the current format under the lock. `compact` given the
    // *full* log folds nothing — it's just a normalized rewrite of log + baseline
    // (the baseline was read through the format-compat path, so it's already
    // current in memory).
    store.compact(&snap.baseline, &snap.log)?;
    for (id, count) in &report {
        println!("migrated `{id}`: {count} event(s)");
    }
    println!("Done.");
    Ok(())
}

/// `--schema`/`--rename`/`--set-type-if-none`: rename stray columns, type the
/// untyped where the user said to, then apply every LOSSLESS fix toward the
/// declared schemas, rewriting the value where it is stored (the latest
/// `Create`/`Update` that set it, else the baseline task). Remaining
/// violations are listed with a pointer — never guessed.
fn repair_schema(
    store: &impl EventStore,
    rename: Option<&str>,
    set_type_if_none: Option<&str>,
) -> Result<(), DynError> {
    let config = store.config();
    let mut log = store.load_mutations()?;
    let mut baseline = store.load_baseline()?;

    let renamed_count = match rename {
        Some(spec) => {
            let outcome = apply_rename(&mut log, &mut baseline, spec, config)?;
            println!(
                "renamed `{}` -> `{}`: {} record(s)",
                outcome.old, outcome.new, outcome.moved
            );
            if outcome.kept > 0 {
                println!(
                    "kept `{}` on {} record(s): the value doesn't name a declared task type \
                     (repair never writes data the schema would reject)",
                    outcome.old, outcome.kept
                );
            }
            outcome.moved
        }
        None => 0,
    };

    let typed = match set_type_if_none {
        Some(type_name) => backfill_type(&mut log, &mut baseline, type_name, config)?,
        None => Vec::new(),
    };
    for line in &typed {
        println!("typed {line}");
    }

    let fixes = apply_lossless_fixes(&mut log, &mut baseline, config);
    for line in &fixes {
        println!("fixed {line}");
    }

    if renamed_count > 0 || !typed.is_empty() || !fixes.is_empty() {
        store.compact(&baseline, &log)?;
    } else {
        println!("Nothing to fix.");
    }

    // What remains is ambiguous by definition — report it actionably.
    let state = Engine::materialize_state(baseline, log, &config.workflow.done_status);
    let remaining = schema_conformance_report(&state, config);
    if !remaining.is_empty() {
        println!(
            "{} task(s) still don't conform (fix each with one `ta update <id> field=value …`, \
             which must bring the whole task into conformance):",
            remaining.len()
        );
        for line in &remaining {
            println!("  {line}");
        }
    }
    Ok(())
}

/// One `--rename` outcome: the (display) destination, the source, how many
/// records moved, and how many were KEPT because converting them would have
/// violated the schema (type destinations only).
struct RenameOutcome {
    new: String,
    old: String,
    moved: usize,
    kept: usize,
}

/// Move one field to a new name across every event payload and baseline task.
/// The spec is assignment-style `NEW=OLD` (`severity=sev` moves `sev`'s values
/// under `severity`), one pair per invocation. A record already carrying `new`
/// keeps it (the stray `old` is left for a human — merging values would be a
/// guess). The destination's declared-kind coercion happens in the
/// lossless-fix pass that always follows.
///
/// The TASK-TYPE field is a legal destination (either spelling; stored under
/// the canonical key), for migrating a de-facto discriminator column
/// (`category=bug`) into real task types — but only records whose value names
/// a DECLARED type convert: repair never writes data the schema would reject.
/// The status field stays guarded (every task already carries a status, so a
/// rename there would mostly skip-and-confuse).
fn apply_rename(
    log: &mut [MutationEvent],
    baseline: &mut [TaskState],
    spec: &str,
    config: &Config,
) -> Result<RenameOutcome, DynError> {
    let Some((new, old)) = spec
        .split_once('=')
        .filter(|(n, o)| !n.is_empty() && !o.is_empty())
    else {
        return Err(format!("invalid `--rename {spec}` (expected NEW=OLD)").into());
    };
    if old == new {
        return Err(format!("`--rename {spec}`: old and new are the same name").into());
    }
    let workflow = &config.workflow;
    let to_type = new == TASK_TYPE_KEY || new == workflow.type_field;
    if !to_type {
        if RESERVED_FIELD_KEYS.contains(&new) {
            return Err(
                format!("can't rename onto `{new}`: it is a reserved/computed field name").into(),
            );
        }
        if new == crate::model::STATUS_KEY || new == workflow.status_field {
            return Err(format!(
                "can't rename onto `{new}`: it is the status field — set it per task with \
                 `ta update` instead"
            )
            .into());
        }
    }
    // Type destination: store the canonical key, and convert only values that
    // name a declared type — the rest keep their old column, reported.
    let target = if to_type { TASK_TYPE_KEY } else { new };
    let converts = |value: &Value| -> bool {
        !to_type
            || value
                .as_str()
                .is_some_and(|name| config.task_types.types.contains_key(name))
    };
    let (mut moved, mut kept) = (0, 0);
    for event in log.iter_mut() {
        if matches!(
            event.op,
            OpType::Create | OpType::Update | OpType::Append | OpType::Add | OpType::Remove
        ) && event.payload.contains_key(old)
            && !event.payload.contains_key(target)
        {
            if !event.payload.get(old).is_some_and(&converts) {
                kept += 1;
                continue;
            }
            if let Some(value) = event.payload.remove(old) {
                event.payload.insert(target.to_string(), value);
                moved += 1;
            }
        }
    }
    for task in baseline.iter_mut() {
        if task.custom_fields.contains_key(old) && !task.custom_fields.contains_key(target) {
            if !task.custom_fields.get(old).is_some_and(&converts) {
                kept += 1;
                continue;
            }
            if let Some(value) = task.custom_fields.remove(old) {
                task.custom_fields.insert(target.to_string(), value);
                moved += 1;
            }
        }
    }
    Ok(RenameOutcome {
        new: new.to_string(),
        old: old.to_string(),
        moved,
        kept,
    })
}

/// `--set-type-if-none`: stamp the user's chosen (declared) type onto every
/// task that has NONE, written onto the task's first `Create` event (else the
/// baseline task) so the log reads as if the task was always typed. An
/// EXPLICIT migration choice — repair never infers a type, even when only one
/// is declared: the user may be migrating gradually or keeping tasks untyped.
fn backfill_type(
    log: &mut [MutationEvent],
    baseline: &mut [TaskState],
    type_name: &str,
    config: &Config,
) -> Result<Vec<String>, DynError> {
    if !config.task_types.types.contains_key(type_name) {
        return Err(format!(
            "`{type_name}` is not a declared task type (declared: {})",
            config
                .task_types
                .types
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }
    let mut report = Vec::new();
    let state = Engine::materialize_state(
        baseline.to_vec(),
        log.to_vec(),
        &config.workflow.done_status,
    );
    for task in state.values() {
        if task.custom_fields.contains_key(TASK_TYPE_KEY) {
            continue;
        }
        if stamp_new_field(
            log,
            baseline,
            &task.id,
            TASK_TYPE_KEY,
            &Value::String(type_name.to_string()),
        ) {
            report.push(format!(
                "`{}` on `{}`: set to `{type_name}`",
                config.workflow.type_field, task.id
            ));
        }
    }
    Ok(report)
}

/// Write a NEW field onto the record that establishes the task — its first
/// `Create` event, else its baseline entry — so the log reads as if the field
/// was always there. Shared by the type backfill and the default stamping.
fn stamp_new_field(
    log: &mut [MutationEvent],
    baseline: &mut [TaskState],
    id: &str,
    key: &str,
    value: &Value,
) -> bool {
    if let Some(event) = log
        .iter_mut()
        .find(|e| matches!(e.op, OpType::Create) && e.task_id == id)
    {
        event.payload.insert(key.to_string(), value.clone());
        return true;
    }
    if let Some(base) = baseline.iter_mut().find(|t| t.id == id) {
        base.custom_fields.insert(key.to_string(), value.clone());
        return true;
    }
    false
}

/// Every deterministic, lossless VALUE fix toward the declared schemas, applied
/// where the offending value is stored: declared-field coercions on the
/// materialized value (numeric strings, scalars to singletons, bool strings,
/// common date formats to RFC 3339). Tasks without a type are untouched —
/// typing them is `--set-type-if-none`'s explicit job.
fn apply_lossless_fixes(
    log: &mut [MutationEvent],
    baseline: &mut [TaskState],
    config: &Config,
) -> Vec<String> {
    let mut report = Vec::new();
    let state = Engine::materialize_state(
        baseline.to_vec(),
        log.to_vec(),
        &config.workflow.done_status,
    );
    for task in state.values() {
        let Some(def) = task
            .custom_fields
            .get(TASK_TYPE_KEY)
            .and_then(Value::as_str)
            .and_then(|name| config.task_types.types.get(name))
        else {
            continue;
        };
        for (name, schema) in &def.fields {
            let key = crate::schema::declared_field_key(name, &config.workflow.status_field);
            let Some(value) = task.custom_fields.get(key) else {
                // A missing REQUIRED field with a declared default is stamped
                // (onto the establishing record, like the type backfill) —
                // the deterministic half of "missing required"; without a
                // default it stays a suggestion.
                if schema.required() {
                    if let Some(default) = schema.default_value() {
                        if stamp_new_field(log, baseline, &task.id, key, default) {
                            report.push(format!(
                                "`{name}` on `{}`: stamped default {default}",
                                task.id
                            ));
                        }
                    }
                }
                continue;
            };
            let Ok(kind) = FieldKind::parse(schema.kind_str()) else {
                continue;
            };
            if kind.matches_value(value, schema.values()) {
                continue;
            }
            let Some(fixed) = lossless_fix(value, &kind, schema.values()) else {
                continue;
            };
            if rewrite_field_source(log, baseline, &task.id, key, &fixed) {
                report.push(format!("`{name}` on `{}`: {value} -> {fixed}", task.id));
            }
        }
    }
    report
}

/// A deterministic, lossless conversion of `value` toward `kind`, or `None`
/// (ambiguous — never guess). The schema-aware write coercions cover most of
/// it; repair adds normalizing common date formats to RFC 3339.
fn lossless_fix(value: &Value, kind: &FieldKind, values: &[String]) -> Option<Value> {
    if let Some(coerced) = crate::schema::coerce_value(value, kind, None) {
        if kind.matches_value(&coerced, values) {
            return Some(coerced);
        }
    }
    if matches!(kind, FieldKind::Datetime) {
        let text = value.as_str()?;
        let normalized = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
                    .or_else(|_| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
                    .ok()
            })?;
        return Some(Value::String(normalized.and_utc().to_rfc3339()));
    }
    None
}

/// Rewrite the record the field's CURRENT value comes from: the latest
/// `Create`/`Update` event carrying the key (non-null), else the baseline
/// task. `false` when neither holds it (e.g. a value built up by appends) —
/// such a fix stays a suggestion.
fn rewrite_field_source(
    log: &mut [MutationEvent],
    baseline: &mut [TaskState],
    id: &str,
    key: &str,
    fixed: &Value,
) -> bool {
    for event in log.iter_mut().rev() {
        if matches!(event.op, OpType::Create | OpType::Update)
            && event.task_id == id
            && event.payload.get(key).is_some_and(|v| !v.is_null())
        {
            event.payload.insert(key.to_string(), fixed.clone());
            return true;
        }
    }
    if let Some(task) = baseline.iter_mut().find(|t| t.id == id) {
        if task.custom_fields.contains_key(key) {
            task.custom_fields.insert(key.to_string(), fixed.clone());
            return true;
        }
    }
    false
}
