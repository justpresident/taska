//! `ta update` - set (`=`), accumulate (`+=`), or remove (`-=`) fields on a task.

use crate::cli::parse_field_ops;
use crate::error::DynError;
use crate::format::{seq_tag, task_ref, want_color};
use crate::schema::{canonicalize_field_pairs, canonicalize_fields};
use crate::storage::EventStore;

pub fn cmd_update(
    store: &impl EventStore,
    id: &str,
    fields: &[String],
    new_field: bool,
    guard: &[String],
) -> Result<(), DynError> {
    let mut ops = parse_field_ops(fields)?;
    // Display names map onto their canonical storage keys: a renamed `state+=x`
    // must hit the same single-valued rejection that `status+=x` does under the
    // default name; `raw` keeps its keys aligned with `set` for coercion.
    let workflow = &store.config().workflow;
    for map in [&mut ops.set, &mut ops.raw] {
        canonicalize_fields(map, workflow)?;
    }
    // The `+=`/`-=` operands are ordered pairs (repeats preserved), so they
    // canonicalize keys through the pairs-aware variant.
    canonicalize_field_pairs(&mut ops.append, workflow)?;
    canonicalize_field_pairs(&mut ops.subtract, workflow)?;
    let outcome = crate::action::write::update(store, id, &ops, new_field, guard)?;
    let color = want_color(false);
    if outcome.written.is_empty() {
        println!("{} already up to date - no changes", task_ref(id, color));
    } else {
        let seq = outcome.written.last().map_or(0, |e| e.seq);
        println!(
            "{} Updated task {}",
            seq_tag(seq, color),
            task_ref(id, color)
        );
    }
    if new_field && outcome.new_fields.is_empty() {
        eprintln!("warning: --new-field had no effect - no new field names were introduced");
    }
    Ok(())
}
