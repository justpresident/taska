//! Git custom merge driver: structurally union diverged event logs.
//!
//! Both branches only ever append events with unique ids, so a merge is simply
//! the union of all events, re-sorted onto a single deterministic timeline.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};

use crate::error::DynError;
use crate::model::MutationEvent;

/// Merge `ancestor` + `current` + `incoming` event files into `current` (Git's
/// `%A`). Returns Ok(()) on a clean structural resolution.
pub fn execute_git_merge(ancestor: &str, current: &str, incoming: &str) -> Result<(), DynError> {
    // BTreeMap keeps the unified timeline sorted by (timestamp, id).
    let mut unified_timeline: BTreeMap<String, MutationEvent> = BTreeMap::new();

    let mut extract_events = |file_path: &str| -> Result<(), DynError> {
        if std::path::Path::new(file_path).exists() {
            let file = File::open(file_path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if !line.trim().is_empty() {
                    let event: MutationEvent = serde_json::from_str(&line)?;
                    // Compound key: chronological, with id as a tiebreaker so
                    // same-microsecond events never collide.
                    let compound_key = format!("{}_{}", event.timestamp.to_rfc3339(), event.id);
                    unified_timeline.insert(compound_key, event);
                }
            }
        }
        Ok(())
    };

    extract_events(ancestor)?;
    extract_events(current)?;
    extract_events(incoming)?;

    // Overwrite Git's %A scratch file with the unified timeline.
    let mut target = File::create(current)?;
    for event in unified_timeline.values() {
        writeln!(target, "{}", serde_json::to_string(event)?)?;
    }
    target.flush()?;

    Ok(())
}
