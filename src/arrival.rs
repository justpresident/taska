//! Per-clone arrival numbers: the cursor `watch`, `history` and `status --current`
//! hand out.
//!
//! A log `seq` is a position in ONE branch's log, and a merge restacks the events
//! it brings in onto fresh seqs - so after a pull an event this clone has never
//! seen can sit at or below a cursor, and an event it has seen can move above it.
//! A seq cursor would then skip the first and repeat the second.
//!
//! An arrival number instead counts events in the order they first appeared in
//! THIS clone's log: each event (identified by its content, see [`content_key`])
//! gets the next number the first time a read sees it, and keeps it forever,
//! however later merges renumber its seq. A cursor of arrival numbers therefore
//! names exactly the events this clone had seen, so `--since <cursor>` resumes
//! with nothing skipped or repeated.
//!
//! The numbers live in [`ARRIVALS_FILE`] beside the log in the store's data
//! directory. They are per-clone by definition, so the file is never committed:
//! it is one of [`crate::storage::PER_CLONE_FILES`], which
//! [`crate::scm::ensure_ignored`] keeps in the SCM's ignore files. On a store
//! that has no file yet, the existing events are seeded with their own seqs, so
//! cursors handed out before this file existed stay valid; a log that has never
//! been merged keeps arrival == seq.
//!
//! The cursor is the file's HIGH-WATER arrival number, not the highest number of
//! an event still in the log, so it never goes backwards: when events leave the
//! log (an undo truncating uncommitted events, `resolve` pruning orphans,
//! compaction folding the prefix), their entries are pruned but the high-water is
//! kept as a bare `{"n": N}` entry, and the next new event is numbered above it.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DynError;
use crate::model::MutationEvent;

/// The arrival-number file, beside the log in the store's data directory.
pub const ARRIVALS_FILE: &str = "arrivals.jsonl";

/// An event's identity across branches.
///
/// That is its full content except the parts that are positions in ONE
/// branch's log - `seq` itself, and an undo compensation's
/// `_meta.undoes` (the seq of the event it undoes, which the merge driver rewrites
/// when it restacks that event). Everything else (timestamp to the nanosecond, op,
/// task, payload, the rest of `_meta`) is what the writer recorded and survives
/// every merge unchanged.
pub fn content_key(event: &MutationEvent) -> String {
    let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.remove("seq");
        if let Some(Value::Object(meta)) = map.get_mut(crate::model::META_KEY) {
            meta.remove(crate::model::UNDOES_META_KEY);
            if meta.is_empty() {
                map.remove(crate::model::META_KEY);
            }
        }
    }
    value.to_string()
}

/// A short, stable digest of [`content_key`] for the arrivals file: 64-bit
/// FNV-1a, hex. Stable across builds and platforms (unlike `std`'s hasher), so a
/// file written by one `ta` reads back identically in another.
fn digest(event: &MutationEvent) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = content_key(event)
        .bytes()
        .fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME));
    format!("{hash:016x}")
}

#[derive(Serialize, Deserialize)]
struct Entry {
    n: u64,
    /// The event's digest; empty for the high-water marker kept by a prune.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    k: String,
}

/// The arrival numbers of a log, plus the cursor: the high-water number this
/// clone has ever handed out (never lower than any number in `numbers`).
pub struct Numbered {
    pub numbers: Vec<u64>,
    pub cursor: u64,
}

/// The arrival number of every event in `log`, in log order.
///
/// Records numbers for events this clone has not seen before. `path` is the store's
/// [`ARRIVALS_FILE`]; the file is created (seeded from the log's seqs) on first
/// use, appended to as new events arrive, and rewritten without stale entries
/// once compaction has folded most of what it records into the baseline.
pub fn arrivals(path: &Path, log: &[MutationEvent]) -> Result<Vec<u64>, DynError> {
    Ok(number(path, log, false)?.numbers)
}

/// [`arrivals`] plus the cursor.
///
/// With `prune`, the file is rewritten to hold only the events still in `log`
/// plus the high-water marker - for callers that just removed events from the
/// log (undo, resolve, compaction).
pub fn number(path: &Path, log: &[MutationEvent], prune: bool) -> Result<Numbered, DynError> {
    let fresh = !path.exists();
    // Read-write, not append: `rewrite` truncates through this handle, which a
    // Windows append handle (opened without FILE_WRITE_DATA) can't do.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // Concurrent readers may both see a new event; the lock makes one of them
    // number it and the other read that number back.
    file.lock()?;

    let mut known: HashMap<String, u64> = HashMap::new();
    let mut max = 0;
    file.seek(SeekFrom::Start(0))?;
    for line in BufReader::new(&file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // A torn or foreign line can't name an arrival; skipping it only means
        // its event gets a fresh number (reported once more), never a skip.
        if let Ok(entry) = serde_json::from_str::<Entry>(&line) {
            max = max.max(entry.n);
            if !entry.k.is_empty() {
                known.entry(entry.k).or_insert(entry.n);
            }
        }
    }

    let mut numbers = Vec::with_capacity(log.len());
    let mut new = Vec::new();
    for event in log {
        let k = digest(event);
        let n = if let Some(&n) = known.get(&k) {
            n
        } else {
            // Seed a brand-new file with the events' own seqs, so existing seq
            // cursors keep meaning the same events.
            let n = if fresh {
                event.seq.max(max + 1)
            } else {
                max + 1
            };
            max = n;
            known.insert(k.clone(), n);
            new.push(Entry { n, k });
            n
        };
        numbers.push(n);
    }

    if prune || known.len() > 2 * log.len() + PRUNE_SLACK {
        rewrite(&mut file, log, &numbers, max)?;
    } else if !new.is_empty() {
        file.seek(SeekFrom::End(0))?;
        for entry in &new {
            writeln!(file, "{}", serde_json::to_string(entry)?)?;
        }
        file.flush()?;
    }
    Ok(Numbered {
        numbers,
        cursor: max,
    })
}

/// Entries kept beyond the retained log before the file is rewritten.
const PRUNE_SLACK: usize = 1000;

/// Rewrite the file with only the events still in the log - entries for events
/// that left it can never be looked up again - plus a bare high-water marker, so
/// the cursor and the next number never go backwards.
fn rewrite(
    file: &mut File,
    log: &[MutationEvent],
    numbers: &[u64],
    high_water: u64,
) -> Result<(), DynError> {
    let mut body = serde_json::to_string(&Entry {
        n: high_water,
        k: String::new(),
    })?;
    body.push('\n');
    for (event, &n) in log.iter().zip(numbers) {
        body.push_str(&serde_json::to_string(&Entry {
            n,
            k: digest(event),
        })?);
        body.push('\n');
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(body.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// The highest of `numbers` (0 when empty) - the cursor of a store with no
/// arrivals file, whose numbers are the events' seqs.
pub fn max_of(numbers: &[u64]) -> u64 {
    numbers.iter().copied().max().unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::model::OpType;
    use chrono::{DateTime, Utc};
    use serde_json::Map;
    use std::fs;

    fn ev(seq: u64, mins: i64, task: &str) -> MutationEvent {
        let base = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        MutationEvent {
            seq,
            timestamp: base + chrono::Duration::minutes(mins),
            op: OpType::Create,
            task_id: task.to_string(),
            meta: None,
            payload: Map::new(),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("taska-arrival").join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(ARRIVALS_FILE)
    }

    #[test]
    fn a_fresh_file_seeds_arrivals_from_seqs() {
        let path = scratch("seed");
        let log = vec![ev(1, 0, "a"), ev(2, 1, "b"), ev(5, 2, "c")];
        assert_eq!(arrivals(&path, &log).unwrap(), vec![1, 2, 5]);
    }

    #[test]
    fn a_restacked_log_keeps_old_numbers_and_numbers_new_events_after_them() {
        // Before the pull: base, b1, b2 (all seen). The pull restacks b1/b2 above
        // a1/a2, which this clone has never seen.
        let path = scratch("restack");
        let (base, b1, b2) = (ev(1, 0, "base"), ev(2, 5, "b1"), ev(3, 6, "b2"));
        assert_eq!(
            arrivals(&path, &[base.clone(), b1.clone(), b2.clone()]).unwrap(),
            vec![1, 2, 3]
        );

        let (a1, a2) = (ev(2, 1, "a1"), ev(3, 2, "a2"));
        let (mut b1r, mut b2r) = (b1, b2);
        b1r.seq = 4;
        b2r.seq = 5;
        let pulled = [base, a1, a2, b1r, b2r];
        assert_eq!(
            arrivals(&path, &pulled).unwrap(),
            vec![1, 4, 5, 2, 3],
            "seen events keep their numbers; the unseen ones arrive after the cursor (3)"
        );
    }

    #[test]
    fn numbers_are_stable_across_reads_and_ignore_seq() {
        let path = scratch("stable");
        let log = vec![ev(1, 0, "a"), ev(2, 1, "b")];
        let first = arrivals(&path, &log).unwrap();
        let mut renumbered = log;
        renumbered[1].seq = 9;
        assert_eq!(arrivals(&path, &renumbered).unwrap(), first);
    }

    #[test]
    fn removing_events_prunes_their_entries_but_never_lowers_the_cursor() {
        let path = scratch("prune");
        let log = vec![ev(1, 0, "a"), ev(2, 1, "b"), ev(3, 2, "c")];
        assert_eq!(number(&path, &log, false).unwrap().cursor, 3);
        // An undo truncates c: its entry goes, the cursor stays at 3.
        let truncated = &log[..2];
        let after = number(&path, truncated, true).unwrap();
        assert_eq!((after.numbers.clone(), after.cursor), (vec![1, 2], 3));
        let body = fs::read_to_string(&path).unwrap();
        assert_eq!(
            body.lines().count(),
            3,
            "marker + a + b, no entry for c:\n{body}"
        );
        // The next event is numbered above the high-water, not reusing 3.
        let next = [truncated, &[ev(3, 9, "d")]].concat();
        assert_eq!(number(&path, &next, false).unwrap().numbers, vec![1, 2, 4]);
    }
}
