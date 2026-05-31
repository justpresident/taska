//! Persistence layer.
//!
//! [`EventStore`] is the abstraction the rest of the program depends on; it
//! says *what* a store can do, not *how*. [`FileStore`] is the concrete,
//! fd-locked JSONL-on-disk implementation. Depending on the trait keeps the
//! command and engine layers ignorant of the storage mechanism and lets tests
//! substitute an in-memory fake.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fd_lock::RwLock;

use crate::config::Config;
use crate::error::DynError;
use crate::model::{MutationEvent, TaskState};

/// The persistence operations the application relies on.
pub trait EventStore {
    /// Read the compacted baseline snapshot.
    fn load_baseline(&self) -> Result<Vec<TaskState>, DynError>;
    /// Read every event from the active mutation log.
    fn load_mutations(&self) -> Result<Vec<MutationEvent>, DynError>;
    /// Append events to the end of the log without rewriting existing lines.
    fn append_events(&self, new_events: &[MutationEvent]) -> Result<(), DynError>;
    /// Replace the baseline with `baseline` and rewrite the log to contain
    /// exactly `retained` (the recent events kept for merge reconciliation).
    fn compact(&self, baseline: &[TaskState], retained: &[MutationEvent]) -> Result<(), DynError>;
}

/// JSONL-on-disk event store rooted at a repo's `.taska` directory.
pub struct FileStore {
    pub base_dir: PathBuf,
    config: Config,
}

impl FileStore {
    /// Open the store at `base_dir`, loading its `config.toml` (defaults if the
    /// file is absent).
    fn at(base_dir: PathBuf) -> Result<Self, DynError> {
        let config = Config::load(&base_dir.join("config.toml"))?;
        Ok(FileStore { base_dir, config })
    }

    /// Locate an existing `.taska` directory by walking up from the current dir.
    pub fn discover() -> Result<Self, DynError> {
        let mut dir = std::env::current_dir()?;
        loop {
            if dir.join(".taska").is_dir() {
                return FileStore::at(dir.join(".taska"));
            }
            if !dir.pop() {
                return Err("No .taska directory found. Run `ta init` first."
                    .to_string()
                    .into());
            }
        }
    }

    /// Idempotently provision the store at `base_dir`: create the directory,
    /// write the default `config.toml` if absent, then create the *configured*
    /// log files if absent. Because it loads the existing config first, re-running
    /// it after editing `[store]` paths creates the renamed files. Existing data
    /// is never touched.
    pub fn provision(base_dir: PathBuf) -> Result<Self, DynError> {
        fs::create_dir_all(&base_dir)?;

        // Write the documented default config only if absent, so re-running
        // `ta init` never clobbers a user's edits.
        let config_path = base_dir.join("config.toml");
        if !config_path.exists() {
            fs::write(&config_path, crate::config::default_toml())?;
        }

        let store = FileStore::at(base_dir)?;
        // Touch the (configured) log files so subsequent reads never fail.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(store.mutations_path())?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(store.baseline_path())?;
        Ok(store)
    }

    /// The repository root containing the `.taska` directory.
    pub fn repo_root(&self) -> Option<&Path> {
        self.base_dir.parent()
    }

    /// The loaded configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    fn mutations_path(&self) -> PathBuf {
        self.base_dir.join("mutations.jsonl")
    }

    fn baseline_path(&self) -> PathBuf {
        self.base_dir.join("baseline.jsonl")
    }
}

impl EventStore for FileStore {
    fn load_baseline(&self) -> Result<Vec<TaskState>, DynError> {
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

    fn load_mutations(&self) -> Result<Vec<MutationEvent>, DynError> {
        read_events(&self.mutations_path())
    }

    /// Append-only write path for normal operations. Never rewrites or reorders
    /// existing lines, which is what keeps the log Git-merge-friendly (branches
    /// only ever append).
    fn append_events(&self, new_events: &[MutationEvent]) -> Result<(), DynError> {
        if new_events.is_empty() {
            return Ok(());
        }
        // `append(true)` keeps the write offset pinned to EOF for every write.
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(self.mutations_path())?;

        // OS advisory write lock so concurrent writers can't interleave partial
        // lines into the log.
        let mut lock = RwLock::new(file);
        let mut locked_file = lock.write()?;

        for event in new_events {
            writeln!(locked_file, "{}", serde_json::to_string(event)?)?;
        }
        locked_file.flush()?;
        Ok(())
    }

    /// Unlike normal writes this *does* rewrite the log — that is the whole
    /// point of compaction. The mutation log is held under the exclusive lock
    /// across the baseline swap so a concurrent `append_events` can't slip an
    /// event in between writing the baseline and rewriting the log.
    fn compact(&self, baseline: &[TaskState], retained: &[MutationEvent]) -> Result<(), DynError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // we truncate explicitly under the lock, below
            .open(self.mutations_path())?;
        let mut lock = RwLock::new(file);
        let mut locked_file = lock.write()?;

        // Persist the new baseline first...
        let mut baseline_file = File::create(self.baseline_path())?;
        for state in baseline {
            writeln!(baseline_file, "{}", serde_json::to_string(state)?)?;
        }
        baseline_file.flush()?;

        // ...then rewrite the log with just the retained events under the lock.
        locked_file.set_len(0)?;
        locked_file.seek(SeekFrom::Start(0))?;
        for event in retained {
            writeln!(locked_file, "{}", serde_json::to_string(event)?)?;
        }
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
