//! `ta repair` - bring a store's on-disk format (`--migrate`) or its DATA
//! (`--schema`, `--rename`) up to scratch.
//!
//! The fixes live in [`crate::action::repair`] (the one sanctioned non-append-only
//! writer); this file renders their reports. The review surface is git-native:
//! `git diff` shows exactly what changed, `git restore` of the `.taska` paths
//! reverts it before a commit.

use crate::error::DynError;
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
        render_migrate(&crate::action::repair::migrate(store)?);
    }
    if schema || rename.is_some() || set_type_if_none.is_some() {
        render_schema(&crate::action::repair::schema(
            store,
            rename,
            set_type_if_none,
        )?);
    }
    Ok(())
}

/// Report the `--migrate` outcome: a line per migration, else "up to date".
fn render_migrate(report: &[(&'static str, usize)]) {
    if report.is_empty() {
        println!("Already up to date; nothing to migrate.");
        return;
    }
    for (id, count) in report {
        println!("migrated `{id}`: {count} event(s)");
    }
    println!("Done.");
}

/// Report the `--schema`/`--rename`/`--set-type-if-none` outcome.
fn render_schema(report: &crate::action::repair::SchemaRepairReport) {
    if let Some(o) = &report.rename {
        println!("renamed `{}` -> `{}`: {} record(s)", o.old, o.new, o.moved);
        if o.kept > 0 {
            println!(
                "kept `{}` on {} record(s): the value doesn't name a declared task type \
                 (repair never writes data the schema would reject)",
                o.old, o.kept
            );
        }
    }
    for line in &report.typed {
        println!("typed {line}");
    }
    for line in &report.fixes {
        println!("fixed {line}");
    }
    if !report.changed {
        println!("Nothing to fix.");
    }
    if !report.remaining.is_empty() {
        println!(
            "{} task(s) still don't conform (fix each with one `ta update <id> field=value ...`, \
             which must bring the whole task into conformance):",
            report.remaining.len()
        );
        for line in &report.remaining {
            println!("  {line}");
        }
    }
}
