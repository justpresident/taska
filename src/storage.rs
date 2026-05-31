//! Storage layer: event schemas, file I/O, and OS-level file locking.
//!
//! All mutations flow through an exclusive, fd-locked transaction so parallel
//! processes / threads never interleave partial lines into the event log.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type DynError = Box<dyn std::error::Error>;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum OpType {
    Create,
    Update,
    Delete,
    AddDep,
    RemoveDep,
}

/// A single append-only record in the mutation log.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MutationEvent {
    pub id: String,               // Unique UUIDv4 identifier
    pub timestamp: DateTime<Utc>, // ISO 8601 timeline location
    pub op: OpType,
    pub task_id: String,

    // Catch-all for schema-agnostic field management.
    #[serde(flatten)]
    pub payload: Map<String, Value>,
}

impl MutationEvent {
    pub fn new(op: OpType, task_id: impl Into<String>, payload: Map<String, Value>) -> Self {
        MutationEvent {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            op,
            task_id: task_id.into(),
            payload,
        }
    }
}

/// The materialized final state of a single task (lives only in memory, or as a
/// compacted baseline record).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskState {
    pub id: String,
    pub depends_on: Vec<String>,
    pub custom_fields: Map<String, Value>,
}

pub struct Storage {
    pub base_dir: PathBuf,
}

impl Storage {
    /// Construct a Storage handle rooted at `<dir>/.taska`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Storage {
            base_dir: repo_root.as_ref().join(".taska"),
        }
    }

    /// Discover the `.taska` directory by walking up from the current dir.
    /// Falls back to `./.taska` if none is found (e.g. before `ta init`).
    pub fn discover() -> Result<Self, DynError> {
        let mut dir = std::env::current_dir()?;
        loop {
            if dir.join(".taska").is_dir() {
                return Ok(Storage {
                    base_dir: dir.join(".taska"),
                });
            }
            if !dir.pop() {
                return Err(
                    "No .taska directory found. Run `ta init` first.".to_string().into()
                );
            }
        }
    }

    fn mutations_path(&self) -> PathBuf {
        self.base_dir.join("mutations.jsonl")
    }

    fn baseline_path(&self) -> PathBuf {
        self.base_dir.join("baseline.jsonl")
    }

    /// Run `ta init`: create the directory, empty log files, and wire up Git.
    pub fn init(repo_root: impl AsRef<Path>) -> Result<Self, DynError> {
        let storage = Storage::new(&repo_root);
        fs::create_dir_all(&storage.base_dir)?;
        // Touch the log files so subsequent reads never fail.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(storage.mutations_path())?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(storage.baseline_path())?;
        Ok(storage)
    }

    /// Read every event from the active mutation log.
    pub fn load_mutations(&self) -> Result<Vec<MutationEvent>, DynError> {
        read_events(&self.mutations_path())
    }

    /// Read the compacted baseline snapshot.
    pub fn load_baseline(&self) -> Result<Vec<TaskState>, DynError> {
        let path = self.baseline_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&path)?;
        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                out.push(serde_json::from_str::<TaskState>(&line)?);
            }
        }
        Ok(out)
    }

    /// Acquire the exclusive write lock, hand the caller the current event
    /// new events to the end of the log. This is the only write path for
    /// normal operations and never rewrites or reorders existing lines, which
    /// is what keeps the log Git-merge-friendly (branches only ever append).
    pub fn append_events(&self, new_events: &[MutationEvent]) -> Result<(), DynError> {
        if new_events.is_empty() {
            return Ok(());
        }
        // `append(true)` keeps the write offset pinned to EOF for every write.
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(self.mutations_path())?;

        // Acquire OS advisory write lock so concurrent writers can't interleave
        // partial lines into the log.
        let mut lock = RwLock::new(file);
        let mut locked_file = lock.write()?;

        for event in new_events {
            writeln!(locked_file, "{}", serde_json::to_string(event)?)?;
        }
        locked_file.flush()?;
        Ok(())
    }

    /// Replace the baseline with `states` and clear the active mutation log.
    ///
    /// Unlike normal writes this *does* truncate the log — that is the whole
    /// point of compaction. The mutation log is held under the exclusive lock
    /// across the baseline swap so a concurrent `append_events` can't slip an
    /// event in between writing the baseline and truncating the log.
    pub fn write_baseline(&self, states: &[TaskState]) -> Result<(), DynError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(self.mutations_path())?;
        let mut lock = RwLock::new(file);
        let mut locked_file = lock.write()?;

        // Persist the new baseline first...
        let mut baseline = File::create(self.baseline_path())?;
        for state in states {
            writeln!(baseline, "{}", serde_json::to_string(state)?)?;
        }
        baseline.flush()?;

        // ...then drop the now-folded mutations under the held lock.
        locked_file.set_len(0)?;
        locked_file.seek(SeekFrom::Start(0))?;
        locked_file.flush()?;
        Ok(())
    }
}

fn read_events(path: &Path) -> Result<Vec<MutationEvent>, DynError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path)?;
    let mut out = Vec::new();
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<MutationEvent>(&line) {
            Ok(event) => out.push(event),
            // Gracefully skip corrupt lines from manual editing rather than
            // aborting every command.
            Err(e) => eprintln!("warning: skipping corrupt event on line {}: {}", idx + 1, e),
        }
    }
    Ok(out)
}
