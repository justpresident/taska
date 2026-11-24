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
//! directory. They are per-clone by definition, so the file is git-ignored there,
//! never committed. On a store that has no file yet, the existing events are
//! seeded with their own seqs, so cursors handed out before this file existed
//! stay valid; a log that has never been merged keeps arrival == seq.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DynError;
use crate::model::MutationEvent;

/// The arrival-number file, beside the log in the store's data directory.
pub const ARRIVALS_FILE: &str = "arrivals.jsonl";

/// An event's identity across branches: its full content except the parts that
/// are positions in ONE branch's log - `seq` itself, and an undo compensation's
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
    k: String,
}

/// The arrival number of every event in `log`, in log order, recording numbers
/// for events this clone has not seen before. `path` is the store's
/// [`ARRIVALS_FILE`]; the file is created (seeded from the log's seqs) on first
/// use, appended to as new events arrive, and rewritten without stale entries
/// once compaction has folded most of what it records into the baseline.
pub fn arrivals(path: &Path, log: &[MutationEvent]) -> Result<Vec<u64>, DynError> {
    let fresh = !path.exists();
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
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
            known.entry(entry.k).or_insert(entry.n);
        }
    }

    let mut numbers = Vec::with_capacity(log.len());
    let mut new = Vec::new();
    for event in log {
        let k = digest(event);
        let n = match known.get(&k) {
            Some(&n) => n,
            None => {
                // Seed a brand-new file with the events' own seqs, so existing
                // seq cursors keep meaning the same events.
                let n = if fresh { event.seq.max(max + 1) } else { max + 1 };
                max = n;
                known.insert(k.clone(), n);
                new.push(Entry { n, k });
                n
            }
        };
        numbers.push(n);
    }

    if known.len() > 2 * log.len() + PRUNE_SLACK {
        rewrite(&mut file, log, &numbers)?;
    } else if !new.is_empty() {
        for entry in &new {
            writeln!(file, "{}", serde_json::to_string(entry)?)?;
        }
        file.flush()?;
    }
    Ok(numbers)
}

/// Entries kept beyond the retained log before the file is rewritten.
const PRUNE_SLACK: usize = 1000;

/// Rewrite the file with only the events still in the log - entries for events
/// compaction folded into the baseline can never be looked up again.
fn rewrite(file: &mut File, log: &[MutationEvent], numbers: &[u64]) -> Result<(), DynError> {
    let mut body = String::new();
    for (event, &n) in log.iter().zip(numbers) {
        body.push_str(&serde_json::to_string(&Entry { n, k: digest(event) })?);
        body.push('\n');
    }
    file.set_len(0)?;
    file.write_all(body.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// The cursor a read hands out: the highest arrival number (0 on an empty log).
pub fn cursor(numbers: &[u64]) -> u64 {
    numbers.iter().copied().max().unwrap_or(0)
}

/// Make sure the data directory's `.gitignore` keeps [`ARRIVALS_FILE`] out of
/// git: a taska-written ignore file from before this file existed is replaced
/// with the current one; a user's own gets the line appended. A directory with
/// no `.gitignore` is left alone (`ta init` writes one).
pub fn ensure_ignored(gitignore: &Path, current_body: &str, legacy_body: &str) -> Result<(), DynError> {
    let Ok(body) = fs::read_to_string(gitignore) else {
        return Ok(());
    };
    if body.lines().any(|l| l.trim() == ARRIVALS_FILE) {
        return Ok(());
    }
    if body == legacy_body {
        fs::write(gitignore, current_body)?;
    } else {
        let sep = if body.is_empty() || body.ends_with('\n') { "" } else { "\n" };
        fs::write(gitignore, format!("{body}{sep}{ARRIVALS_FILE}\n"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OpType;
    use chrono::{DateTime, Utc};
    use serde_json::Map;

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
        assert_eq!(arrivals(&path, &[base.clone(), b1.clone(), b2.clone()]).unwrap(), vec![1, 2, 3]);

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
    fn ensure_ignored_upgrades_the_legacy_body_and_appends_to_a_users_file() {
        let dir = scratch("ignore").parent().unwrap().to_path_buf();
        let gi = dir.join(".gitignore");
        fs::write(&gi, "merge-conflict.json\n").unwrap();
        ensure_ignored(&gi, "merge-conflict.json\narrivals.jsonl\n", "merge-conflict.json\n").unwrap();
        assert_eq!(fs::read_to_string(&gi).unwrap(), "merge-conflict.json\narrivals.jsonl\n");

        fs::write(&gi, "target/").unwrap();
        ensure_ignored(&gi, "x", "merge-conflict.json\n").unwrap();
        assert_eq!(fs::read_to_string(&gi).unwrap(), "target/\narrivals.jsonl\n");
    }
}
