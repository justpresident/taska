//! `ta prime` - print a config-tailored agent primer for this store.
//!
//! The structured facts come from [`crate::action::prime`]; this file renders
//! them into a markdown guide (human) and passes the same facts to `render_args`
//! as JSON (`--format json`). The guide is plain text - no color - so it satisfies the
//! output-consistency contract trivially. It reads THIS store's vocabulary, so a
//! renamed status field or a freshly declared type is reflected automatically.

use crate::action::prime::{examples, prime, FieldFacts, PrimeExamples, PrimeFacts, TypeFacts};
use crate::cli::render_warnings;
use crate::error::DynError;
use crate::format::{render_args, OutputArgs};
use crate::storage::EventStore;

pub fn cmd_prime(store: &impl EventStore, output: &OutputArgs) -> Result<(), DynError> {
    let outcome = prime(store)?;
    for warning in render_warnings(&outcome.warnings) {
        eprintln!("{warning}");
    }
    for line in render_args(
        output,
        || render_guide(&outcome.facts),
        // The facts are a plain serializable struct, so `to_value` can't fail in
        // practice; `unwrap_or_default` mirrors the serialization idiom in `render_args`.
        || serde_json::to_value(&outcome.facts).unwrap_or_default(),
    ) {
        println!("{line}");
    }
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

/// One task type as a bullet: ``  - `name`: required ...; optional ....``
///
/// The status field is omitted - it has its own dedicated line above, so
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

/// Align a `(command, comment)` cheat-sheet: pad commands to a common width so
/// the `# comment`s line up regardless of this store's status/type name lengths.
fn align(cmds: &[(String, String)]) -> String {
    let width = cmds
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    cmds.iter()
        .map(|(c, note)| format!("{c:<width$}  # {note}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The read/query cheat-sheet (config-tailored filter example).
fn read_commands(ex: &PrimeExamples) -> String {
    align(&[
        (
            "ta list --ready".to_string(),
            "actionable now: not done, all deps done".to_string(),
        ),
        ("ta list --open".to_string(), "the open backlog".to_string()),
        (
            "ta show <id> --full".to_string(),
            "one task - every field, full notes".to_string(),
        ),
        (
            format!("ta list {}", ex.filter),
            "filter: = != =~ !~ > >= < <=".to_string(),
        ),
        (
            "ta dep tree".to_string(),
            "dependency tree (dep plan <goal> = ordered plan)".to_string(),
        ),
        ("ta status".to_string(), "counts".to_string()),
    ])
}

/// The write cheat-sheet (config-tailored create/claim/close/link/note).
fn write_commands(f: &PrimeFacts, ex: &PrimeExamples) -> String {
    let (sf, tf) = (&f.status_field, &f.type_field);
    align(&[
        (
            format!("ta create <id> {tf}={} {}", ex.type_name, ex.req_example),
            format!("file work ({sf} defaults to `{}`)", f.default_status),
        ),
        (
            format!("ta update <id> {sf}={}", ex.claim),
            "set a field: = set, += append, -= remove, field= clears".to_string(),
        ),
        (
            format!(
                "ta update <id> {sf}={} --if {sf}={}",
                ex.claim, f.default_status
            ),
            "claim atomically: applies only if still current (exit 3 if you lost)".to_string(),
        ),
        (
            format!("ta update <id> {sf}={}", f.done_status),
            "close it".to_string(),
        ),
        (
            format!("ta dep add <id> {}=<other>", ex.blocker),
            "record a prerequisite".to_string(),
        ),
        (
            "ta update <id> notes+=\"...\"".to_string(),
            "append a note (here and on related tasks)".to_string(),
        ),
    ])
}

/// The four "## Schema" bullets, config-tailored: status field + values (or
/// free-form), declared types with their fields (or free-form), the relationship
/// types, and the configured `ta list` columns.
fn schema_section(f: &PrimeFacts) -> String {
    let (sf, tf) = (&f.status_field, &f.type_field);

    let status_line = if f.statuses.is_empty() {
        format!(
            "- Status field `{sf}`: free-form (any value); done = `{}`, create defaults to `{}`.",
            f.done_status, f.default_status
        )
    } else {
        format!(
            "- Status field `{sf}`: {} (done = `{}`, create defaults to `{}`).",
            f.statuses.join(" | "),
            f.done_status,
            f.default_status
        )
    };

    let type_block = if f.task_types.is_empty() {
        "- Types: none declared - tasks are free-form (any field name is accepted).".to_string()
    } else {
        let mut block = format!(
            "- Type field `{tf}=` (untyped tasks: {}); declared types:",
            f.untyped_tasks
        );
        for t in &f.task_types {
            block.push('\n');
            block.push_str(&describe_type(t, sf));
        }
        block
    };

    let rels = f
        .relationships
        .iter()
        .map(|r| {
            let note = if r.inverse.is_empty() {
                ", one-way".to_string()
            } else if r.inverse == r.name {
                ", symmetric".to_string()
            } else {
                format!(", inverse `{}`", r.inverse)
            };
            format!("`{}` ({}{note})", r.name, r.kind)
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "{status_line}\n\
         {type_block}\n\
         - Relationships (for `ta dep`): {rels}.\n\
         - `ta list` columns (config-driven): {}.",
        f.columns.join(", ")
    )
}

/// Render the config-tailored markdown primer.
fn render_guide(f: &PrimeFacts) -> String {
    let sf = &f.status_field;
    let ex = examples(f);
    let s = &f.summary;

    format!(
        "# taska (`ta`) - task & dependency tracker for this repo\n\
         \n\
         Tasks live in an append-only log in `.taska/`, replayed to current state; branches \
         reconcile automatically via a git merge driver. Drive the store ONLY through `ta` \
         (never hand-edit `.taska/*.jsonl`), and commit the `.taska/` change in the same \
         commit as the code it describes.\n\
         \n\
         ## Schema (dynamic - defined by `.taska/config.toml`, not hardcoded)\n\
         Field names, statuses, types, and relationships are THIS store's config; another \
         repo may differ, so `ta prime` each new store.\n\
         {schema}\n\
         \n\
         ## Read / query\n\
         ```bash\n\
         {read_block}\n\
         ```\n\
         \n\
         ## Filing & tracking tasks\n\
         File a task for each distinct piece of work - one per feature/bug, before or as you \
         start it. Give `notes` enough for someone else to act without you: the goal and \
         intended approach/implementation details, any open or design questions, and the \
         context - for long or multi-line values, read them from stdin (`notes=@-`) or a file \
         (`notes=@FILE`) on any `ta create`/`ta update` instead of quoting on the command line \
         (`+=` takes `@` too). Set prerequisites with `ta dep add`, and append to related tasks \
         as things change so the trail stays current. Don't pass `{sf}=` on create (it defaults \
         to `{default}`); read full notes with `ta show <id> --full`. A field name no task uses \
         yet is rejected unless you add `--new-field` (a typo guard, with a did-you-mean) - reuse \
         existing field names otherwise.\n\
         ```bash\n\
         {write_block}\n\
         ```\n\
         \n\
         Keep git history coherent: the eventlog change and the code it describes belong in one \
         commit - if the store has pending `.taska/` changes unrelated to what you're starting, \
         commit those first.\n\
         \n\
         ## Working a task\n\
         1. `ta list --ready` - pick actionable work.  2. `ta update <id> {sf}={claim}` - \
         claim it.  3. do it, appending notes as you go.  4. `ta update <id> {sf}={done}` - \
         then commit `.taska/` with the code.  5. file the follow-ups you discover.\n\
         \n\
         {open} open ({ready} ready, {blocked} blocked), {closed} closed. Re-run `ta prime` \
         to refresh.\n",
        schema = schema_section(f),
        read_block = read_commands(&ex),
        write_block = write_commands(f, &ex),
        sf = sf,
        default = f.default_status,
        claim = ex.claim,
        done = f.done_status,
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
    use crate::model::ID_KEY;
    use crate::test_support::names::*;
    use crate::test_support::{store_renamed, store_with_schema};

    fn guide() -> String {
        render_guide(&prime(&store_with_schema()).unwrap().facts)
    }

    #[test]
    fn guide_reflects_the_store_vocabulary() {
        let g = guide();
        // The store's actual RENAMED vocabulary must appear in the rendered guide.
        assert!(
            g.contains(&format!("field `{STATUS_FIELD}`")),
            "status field named: {g}"
        );
        assert!(
            g.contains(&format!("{DEFAULT_STATUS} | {MID_STATUS} | {DONE_STATUS}")),
            "status enum values: {g}"
        );
        assert!(
            g.contains(&format!("ta update <id> {STATUS_FIELD}={DONE_STATUS}")),
            "close example uses done status: {g}"
        );
        assert!(
            g.contains(&format!("`{BLOCKER}` (blocker, inverse `{BLOCKER_INV}`)")),
            "relationship described: {g}"
        );
        // The create example lists the required fields (any order) but not status,
        // which `create` stamps from the default.
        assert!(
            g.contains(&format!("ta create <id> {TYPE_FIELD}={TASK_TYPE} ")),
            "create example: {g}"
        );
        assert!(
            g.contains(&format!("{TITLE}=\"...\"")),
            "create lists title: {g}"
        );
        assert!(
            g.contains(&format!("{NOTES}=\"...\"")),
            "create lists notes: {g}"
        );
        assert!(
            !g.contains(&format!("{STATUS_FIELD}=\"...\"")),
            "create omits the stamped status field: {g}"
        );
    }

    #[test]
    fn guide_explains_the_dynamic_schema_and_task_filing() {
        let g = guide();
        // The schema is framed as config-defined, not fixed.
        assert!(
            g.contains("Schema (dynamic"),
            "schema framed as dynamic: {g}"
        );
        assert!(
            g.contains("config-driven")
                && g.contains(&format!("{ID_KEY}, {TITLE}, {STATUS_FIELD}")),
            "names the configured columns: {g}"
        );
        // The task-filing discipline is spelled out: rich notes, open questions,
        // dependencies, cross-task notes.
        assert!(
            g.contains("File a task for each distinct piece of work"),
            "when to file: {g}"
        );
        assert!(
            g.contains("open or design questions"),
            "encourages recording open questions: {g}"
        );
        assert!(g.contains("ta dep add"), "encourages dependencies: {g}");
        assert!(
            g.contains("append to related tasks") && g.contains("notes+="),
            "encourages cross-task notes: {g}"
        );
        // Stdin/file input for long notes, and commit hygiene.
        assert!(
            g.contains("notes=@-") && g.contains("notes=@FILE"),
            "documents stdin/file note input: {g}"
        );
        assert!(
            g.contains("unrelated to what you're starting"),
            "advises flushing unrelated pending changes first: {g}"
        );
    }

    #[test]
    fn guide_is_free_form_when_no_schema_is_declared() {
        let g = render_guide(&prime(&store_renamed()).unwrap().facts);
        assert!(
            g.contains("free-form") && g.contains("any field name is accepted"),
            "explains the free-form fallback: {g}"
        );
    }

    #[test]
    fn guide_has_no_ansi_escapes() {
        // The primer is plain markdown - never colored - so it satisfies the
        // output-consistency contract unconditionally.
        assert!(!guide().contains('\u{1b}'), "guide must be escape-free");
    }

    #[test]
    fn json_serializes_the_facts() {
        let value = serde_json::to_value(prime(&store_with_schema()).unwrap().facts).unwrap();
        assert_eq!(value["status_field"], STATUS_FIELD);
        assert_eq!(value["done_status"], DONE_STATUS);
        assert_eq!(value["statuses"][0], DEFAULT_STATUS);
        assert_eq!(value["summary"]["total"], 0);
        assert!(value["relationships"].is_array());
    }
}
