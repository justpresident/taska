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

/// Turns the current store state (`baseline`, `log`) into the events to append.
///
/// It materializes, validates each draft, and drops no-ops. Returning an error
/// rejects the whole batch (nothing is written); an empty `Vec` means "nothing
/// to do". Run by [`EventStore::append_checked`] so the read→verify→write is one
/// step. See [`crate::cli::vet_events`].
pub type EventBuilder<'a> =
    dyn Fn(&[TaskState], &[MutationEvent]) -> Result<Vec<MutationEvent>, DynError> + 'a;

/// The persistence operations the application relies on.
pub trait EventStore {
    /// The store's loaded configuration. On the trait so command handlers can
    /// reach config (e.g. the workflow/timestamp settings replay needs) through
    /// the abstraction, while tests substitute a store carrying any `Config`.
    fn config(&self) -> &Config;
    /// Read the compacted baseline snapshot.
    fn load_baseline(&self) -> Result<Vec<TaskState>, DynError>;
    /// Read every event from the active mutation log.
    fn load_mutations(&self) -> Result<Vec<MutationEvent>, DynError>;
    /// Append events to the end of the log without rewriting existing lines.
    fn append_events(&self, new_events: &[MutationEvent]) -> Result<(), DynError>;
    /// Atomically append a **verified** batch: read the current state, let
    /// `build` validate it and produce the events to write, then append them
    /// (minting seqs). [`FileStore`] runs the whole sequence under the write
    /// lock, so the check can't race a concurrent writer (a TOCTOU that could,
    /// e.g., create the same task twice). This default is a non-atomic
    /// read→build→append, fine for single-threaded in-memory stores. Returns the
    /// events written (empty = nothing to do).
    fn append_checked(&self, build: &EventBuilder) -> Result<Vec<MutationEvent>, DynError> {
        let baseline = self.load_baseline()?;
        let log = self.load_mutations()?;
        let events = build(&baseline, &log)?;
        self.append_events(&events)?;
        Ok(events)
    }
    /// Replace the baseline with `baseline` and rewrite the log to contain
    /// exactly `retained` (the recent events kept for merge reconciliation).
    fn compact(&self, baseline: &[TaskState], retained: &[MutationEvent]) -> Result<(), DynError>;
    /// Rewrite the mutation log to contain exactly `events`, leaving the baseline
    /// untouched. Used to drop no-op (orphaned) events during `ta resolve`.
    fn replace_mutations(&self, events: &[MutationEvent]) -> Result<(), DynError>;
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
        Ok(Self { base_dir, config })
    }

    /// Locate an existing `.taska` directory by walking up from the current dir.
    pub fn discover() -> Result<Self, DynError> {
        let mut dir = std::env::current_dir()?;
        loop {
            if dir.join(".taska").is_dir() {
                return Self::at(dir.join(".taska"));
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

        // The merge-conflict marker is transient per-clone state, never history.
        let gitignore = base_dir.join(".gitignore");
        if !gitignore.exists() {
            fs::write(&gitignore, "merge-conflict.json\n")?;
        }

        let store = Self::at(base_dir)?;
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

    fn mutations_path(&self) -> PathBuf {
        self.base_dir.join("mutations.jsonl")
    }

    fn baseline_path(&self) -> PathBuf {
        self.base_dir.join("baseline.jsonl")
    }
}

impl EventStore for FileStore {
    fn config(&self) -> &Config {
        &self.config
    }

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
    ///
    /// Sequence numbers are minted here, under the lock: the next `seq` is one
    /// past the largest already in the log (or 1 for a fresh/fully-overlaid log).
    /// Minting under the same lock as the write is what stops two concurrent
    /// writers from handing out the same `seq`.
    fn append_events(&self, drafts: &[MutationEvent]) -> Result<(), DynError> {
        if drafts.is_empty() {
            return Ok(());
        }
        // `append(true)` keeps the write offset pinned to EOF for every write,
        // even after we seek to the start to read the current max `seq`.
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(self.mutations_path())?;

        // OS advisory write lock so concurrent writers can't interleave partial
        // lines into the log or race on sequence assignment.
        let mut lock = RwLock::new(file);
        let mut locked_file = lock.write()?;

        let start = max_seq(&mut locked_file)?.map_or(1, |m| m + 1);
        for (seq, draft) in (start..).zip(drafts) {
            let mut event = draft.clone();
            event.seq = seq;
            writeln!(locked_file, "{}", serde_json::to_string(&event)?)?;
        }
        locked_file.flush()?;
        Ok(())
    }

    /// Atomic verify-then-append: the write lock is held across reading the log,
    /// running `build`, and appending its result, so the validation can't race a
    /// concurrent writer. The log is read **strictly** (an unparseable line is an
    /// error, never skipped) for the same reason [`max_seq`] is — minting over a
    /// log we can't fully read risks a duplicate seq. The baseline is read under
    /// the same lock; it is stable because [`FileStore::compact`] also holds this
    /// lock across its baseline swap.
    fn append_checked(&self, build: &EventBuilder) -> Result<Vec<MutationEvent>, DynError> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(self.mutations_path())?;
        let mut lock = RwLock::new(file);
        let mut locked_file = lock.write()?;

        let log = read_log_strict(&mut locked_file)?;
        let baseline = self.load_baseline()?;
        let events = build(&baseline, &log)?;
        if events.is_empty() {
            return Ok(events);
        }
        let start = log.iter().map(|e| e.seq).max().map_or(1, |m| m + 1);
        for (seq, draft) in (start..).zip(&events) {
            let mut event = draft.clone();
            event.seq = seq;
            writeln!(locked_file, "{}", serde_json::to_string(&event)?)?;
        }
        locked_file.flush()?;
        Ok(events)
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

    /// Rewrite the log in place with exactly `events`, leaving the baseline alone.
    /// Mirrors `compact`'s log-rewrite under the same exclusive fd-lock so a
    /// concurrent `append_events` can't slip an event in mid-rewrite. Unlike
    /// `compact` it never touches the baseline — dropping no-op orphans is
    /// state-neutral, so there is nothing to fold.
    fn replace_mutations(&self, events: &[MutationEvent]) -> Result<(), DynError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // we truncate explicitly under the lock, below
            .open(self.mutations_path())?;
        let mut lock = RwLock::new(file);
        let mut locked_file = lock.write()?;

        locked_file.set_len(0)?;
        locked_file.seek(SeekFrom::Start(0))?;
        for event in events {
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
    // `seq` is the authoritative order, and every write path keeps the file in
    // that order. Verify rather than sort: an out-of-order log is corruption we
    // must surface, not silently repair.
    crate::model::verify_seq_order(&out)?;
    Ok(out)
}

/// Read every event from the open log, **strictly** — an unparseable line is an
/// error (cf. [`read_events`], which tolerates one). Used by the verify-then-write
/// path, which must see the whole log before minting a seq or vetting a draft.
fn read_log_strict(file: &mut File) -> Result<Vec<MutationEvent>, DynError> {
    file.seek(SeekFrom::Start(0))?;
    let mut out = Vec::new();
    for (idx, line) in BufReader::new(&mut *file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: MutationEvent = serde_json::from_str(&line).map_err(|e| {
            format!(
                "mutation log line {} is unparseable ({e}); refusing to read for a write. \
                 Often a stale `ta` binary that predates a newer event type — rebuild/update \
                 `ta`, or run `ta resolve` to rewrite the log, then retry.",
                idx + 1
            )
        })?;
        out.push(event);
    }
    crate::model::verify_seq_order(&out)?;
    Ok(out)
}

/// Largest `seq` in the open log, or `None` when it holds no events.
///
/// Seeks to the start to scan; with the file opened `append(true)`, later writes
/// still land at EOF regardless of where this leaves the read cursor.
///
/// Unlike [`read_events`], which tolerates a corrupt line on *read*, this is
/// **strict**: a line it can't parse is a hard error. Minting `max(seq) + 1` over
/// a log we can only partially read would under-count the max and hand out a
/// **duplicate `seq`** — corrupting the append-only order. The classic trigger is
/// a stale binary that predates a newer `OpType` (e.g. `Append`): it can't
/// deserialize that event, so silently skipping it would mint a seq that already
/// exists. Better to refuse the write and surface the problem.
fn max_seq(file: &mut File) -> Result<Option<u64>, DynError> {
    file.seek(SeekFrom::Start(0))?;
    let mut max: Option<u64> = None;
    for (idx, line) in BufReader::new(&mut *file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: MutationEvent = serde_json::from_str(&line).map_err(|e| {
            format!(
                "refusing to mint a sequence number: mutation log line {} is unparseable \
                 ({e}). This can hand out a duplicate seq and corrupt the log — often a stale \
                 `ta` binary that predates a newer event type. Rebuild/update `ta`, or run \
                 `ta resolve` to rewrite the log, then retry.",
                idx + 1
            )
        })?;
        max = Some(max.map_or(event.seq, |m| m.max(event.seq)));
    }
    Ok(max)
}
