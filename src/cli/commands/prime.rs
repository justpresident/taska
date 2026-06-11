//! `ta prime` — print a config-tailored agent primer for this store.
//!
//! The structured facts come from [`crate::action::prime`]; this file renders
//! them into a markdown guide (human) and passes the same facts to `emit` as JSON
//! (`--format json`). The guide is plain text — no color — so it satisfies the
//! output-consistency contract trivially. It reads THIS store's vocabulary, so a
//! renamed status field or a freshly declared type is reflected automatically.

use crate::action::prime::{prime, FieldFacts, PrimeFacts, TypeFacts};
use crate::cli::print_warnings;
use crate::error::DynError;
use crate::format::{emit, OutputArgs};
use crate::storage::EventStore;

pub fn cmd_prime(store: &impl EventStore, output: &OutputArgs) -> Result<(), DynError> {
    let outcome = prime(store)?;
    print_warnings(&outcome.warnings);
    let human = render_guide(&outcome.facts);
    let value = serde_json::to_value(&outcome.facts)?;
    emit(output, &human, &value);
    Ok(())
}

/// One field rendered for the guide: its name, plus a parenthetical of the enum
/// values (or the declared kind, when it isn't a plain string).
fn describe_field(f: &FieldFacts) -> String {
    if !f.values.is_empty() {
        format!("{} ({})", f.name, f.values.join(" | "))
    } else if f.kind != "string" && f.kind != "any" {
        format!("{} ({})", f.name, f.kind)
    } else {
        f.name.clone()
    }
}

/// One task type as a bullet: ``  - `name`: required …; optional ….``
///
/// The status field is omitted — it has its own dedicated line above, so
/// repeating its enum here would be noise.
fn describe_type(t: &TypeFacts, status_field: &str) -> String {
    let fields = || t.fields.iter().filter(|f| f.name != status_field);
    let req: Vec<String> = fields()
        .filter(|f| f.required)
        .map(describe_field)
        .collect();
    let opt: Vec<String> = fields()
        .filter(|f| !f.required)
        .map(describe_field)
        .collect();
    let mut parts = Vec::new();
    if !req.is_empty() {
        parts.push(format!("required {}", req.join(", ")));
    }
    if !opt.is_empty() {
        parts.push(format!("optional {}", opt.join(", ")));
    }
    let body = if parts.is_empty() {
        "no declared fields".to_string()
    } else {
        parts.join("; ")
    };
    let close = if t.closed { "" } else { " (does not close)" };
    format!("  - `{}`{close}: {body}", t.name)
}

/// The store-specific example tokens woven through the guide (a status to claim
/// with, a type + its required fields to create with, a blocker to link, a field
/// to filter on) — all derived from the config so every example is runnable
/// against THIS store.
struct Examples {
    claim: String,
    type_name: String,
    req_example: String,
    blocker: String,
    filter: String,
}

/// Derive the runnable example tokens from the facts.
fn examples(f: &PrimeFacts) -> Examples {
    let sf = &f.status_field;
    // A representative "claim" status: the first that's neither the default nor
    // done; else the default.
    let claim = f
        .statuses
        .iter()
        .find(|s| *s != &f.default_status && *s != &f.done_status)
        .unwrap_or(&f.default_status)
        .clone();

    // The first declared type + its required fields (minus status, which `create`
    // stamps).
    let first_type = f.task_types.first();
    let type_name = first_type.map_or("task", |t| t.name.as_str()).to_string();
    let req_fields: Vec<String> = first_type
        .map(|t| {
            t.fields
                .iter()
                .filter(|x| x.required && x.name != *sf)
                .map(|x| format!("{}=\"…\"", x.name))
                .collect()
        })
        .unwrap_or_default();
    let req_example = if req_fields.is_empty() {
        "title=\"…\"".to_string()
    } else {
        req_fields.join(" ")
    };

    // The first gating relationship (blocker/hierarchy), for the `dep add` example.
    let blocker = f
        .relationships
        .iter()
        .find(|r| r.kind == "blocker" || r.kind == "hierarchy")
        .or_else(|| f.relationships.first())
        .map_or("depends_on", |r| r.name.as_str())
        .to_string();

    // A filter example: an optional enum field if present, else the status field.
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

    Examples {
        claim,
        type_name,
        req_example,
        blocker,
        filter,
    }
}

/// The command cheat-sheet, comments aligned regardless of how long this store's
/// status/type names make each command.
fn command_block(f: &PrimeFacts, ex: &Examples) -> String {
    let (sf, tf) = (&f.status_field, &f.type_field);
    let (claim, type_name, req_example, blocker, filter) = (
        &ex.claim,
        &ex.type_name,
        &ex.req_example,
        &ex.blocker,
        &ex.filter,
    );
    let commands: Vec<(String, String)> = vec![
        (
            "ta list --ready".to_string(),
            "actionable now: not done, all deps done".to_string(),
        ),
        ("ta list --open".to_string(), "the open backlog".to_string()),
        (
            "ta show <id> --full".to_string(),
            "one task — every field, full notes".to_string(),
        ),
        (
            format!("ta create <id> {tf}={type_name} {req_example}"),
            format!("file work ({sf} defaults to `{}`)", f.default_status),
        ),
        (
            format!("ta update <id> {sf}={claim}"),
            "set fields: = replace, += append, -= remove".to_string(),
        ),
        (
            format!("ta update <id> {sf}={}", f.done_status),
            "close it".to_string(),
        ),
        (
            format!("ta dep add <id> {blocker}=<other>"),
            "link a dep (also: ta dep tree, ta dep plan <goal>)".to_string(),
        ),
        (
            format!("ta list {filter}"),
            "filter: = != =~ !~ > >= < <=".to_string(),
        ),
        ("ta status".to_string(), "counts".to_string()),
    ];
    let width = commands
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    commands
        .iter()
        .map(|(c, note)| format!("{c:<width$}  # {note}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render the config-tailored markdown primer.
fn render_guide(f: &PrimeFacts) -> String {
    let (sf, tf) = (&f.status_field, &f.type_field);
    let ex = examples(f);
    let (claim, type_name, req_example) = (&ex.claim, &ex.type_name, &ex.req_example);

    let statuses = if f.statuses.is_empty() {
        "free-form".to_string()
    } else {
        f.statuses.join(" | ")
    };
    let type_lines = if f.task_types.is_empty() {
        "  - (none declared — `type` is unconstrained)".to_string()
    } else {
        f.task_types
            .iter()
            .map(|t| describe_type(t, sf))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let rel_lines = f
        .relationships
        .iter()
        .map(|r| {
            let note = if r.inverse.is_empty() {
                "one-way".to_string()
            } else if r.inverse == r.name {
                "symmetric".to_string()
            } else {
                format!("inverse `{}`", r.inverse)
            };
            format!("  - `{}` — {} ({note})", r.name, r.kind)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let cmd_block = command_block(f, &ex);
    let s = &f.summary;

    format!(
        "# taska (`ta`) — task tracking for this repo\n\
         \n\
         Tasks live in an append-only event log in `.taska/`, replayed to current \
         state; concurrent edits on different branches merge automatically (a git \
         merge driver). Mutate the store ONLY through `ta` — never hand-edit \
         `.taska/*.jsonl`, and commit the `.taska/` change in the same commit as \
         the code it describes.\n\
         \n\
         ## This store's vocabulary\n\
         - Status: field `{sf}` — {statuses} (done = `{done}`, new tasks default to `{default}`).\n\
         - Types (set with `{tf}=`):\n\
         {type_lines}\n\
         \x20 Untyped tasks: {untyped}.\n\
         - Relationships:\n\
         {rel_lines}\n\
         - Columns `ta list` shows: {columns}.\n\
         \n\
         ## Core commands\n\
         ```bash\n\
         {cmd_block}\n\
         ```\n\
         \n\
         ## Suggested agent loop\n\
         1. `ta list --ready` — pick something actionable.\n\
         2. `ta update <id> {sf}={claim}` — claim it.\n\
         3. Do the work; record progress with `ta update <id> notes+=\"…\"` (or `notes=@-` to read multi-line notes from stdin).\n\
         4. `ta update <id> {sf}={done}` — then commit the `.taska/` change in the same commit as the code.\n\
         5. Found more work? File it: `ta create <id> {tf}={type_name} {req_example}`.\n\
         \n\
         {open} open ({ready} ready, {blocked} blocked), {closed} closed. \
         Re-run `ta prime` any time to refresh this.\n",
        done = f.done_status,
        default = f.default_status,
        untyped = f.untyped_tasks,
        columns = f.columns.join(", "),
        open = s.open,
        ready = s.ready,
        blocked = s.blocked,
        closed = s.closed,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::store_with_schema;

    fn guide() -> String {
        render_guide(&prime(&store_with_schema()).unwrap().facts)
    }

    #[test]
    fn guide_reflects_the_store_vocabulary() {
        let g = guide();
        // The default store's actual vocabulary must appear in the rendered guide.
        assert!(g.contains("field `status`"), "status field named: {g}");
        assert!(
            g.contains("todo | in_progress | closed"),
            "status enum values: {g}"
        );
        assert!(
            g.contains("ta update <id> status=closed"),
            "close example uses done status: {g}"
        );
        assert!(
            g.contains("`depends_on` — blocker (inverse `blocks`)"),
            "relationship described: {g}"
        );
        // The create example lists the required fields (any order) but not status,
        // which `create` stamps from the default.
        assert!(
            g.contains("ta create <id> type=task "),
            "create example: {g}"
        );
        assert!(g.contains("title=\"…\""), "create lists title: {g}");
        assert!(g.contains("notes=\"…\""), "create lists notes: {g}");
        assert!(
            !g.contains("status=\"…\""),
            "create omits the stamped status field: {g}"
        );
    }

    #[test]
    fn guide_has_no_ansi_escapes() {
        // The primer is plain markdown — never colored — so it satisfies the
        // output-consistency contract unconditionally.
        assert!(!guide().contains('\u{1b}'), "guide must be escape-free");
    }

    #[test]
    fn json_serializes_the_facts() {
        let value = serde_json::to_value(prime(&store_with_schema()).unwrap().facts).unwrap();
        assert_eq!(value["status_field"], "status");
        assert_eq!(value["done_status"], "closed");
        assert_eq!(value["statuses"][0], "todo");
        assert_eq!(value["summary"]["total"], 0);
        assert!(value["relationships"].is_array());
    }
}
