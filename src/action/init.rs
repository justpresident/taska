//! `init` action: provision the store and register the git merge drivers.

use std::path::{Path, PathBuf};

use crate::error::DynError;
use crate::git;
use crate::storage::FileStore;

/// Whether `init` reused an existing store or created a new one (with its path).
pub enum InitOutcome {
    Reused(PathBuf),
    Created(PathBuf),
}

/// Provision the store idempotently and (re)register the git merge driver.
///
/// Reuse a discoverable store (so re-running from anywhere in the repo is
/// idempotent), else create one at the SCM root — committed there, the store
/// travels with the repo and every clone's walk-up discovery finds it; only a
/// plain directory (no SCM above) keeps it at the cwd. The driver is always
/// (re)registered, so re-running is how a clone installs it locally.
pub fn init() -> Result<InitOutcome, DynError> {
    let (base_dir, outcome) = if let Ok(existing) = FileStore::discover() {
        let dir = existing.base_dir;
        (dir.clone(), InitOutcome::Reused(dir))
    } else {
        let cwd = std::env::current_dir()?;
        let root = git::scm_root(&cwd).map(Path::to_path_buf).unwrap_or(cwd);
        let dir = root.join(".taska");
        (dir.clone(), InitOutcome::Created(dir))
    };

    // Provision honors the (possibly user-edited) config, creating any newly
    // configured log files — re-running `init` is how a `[store]` path change is
    // applied.
    let store = FileStore::provision(base_dir)?;
    let repo_root = store
        .repo_root()
        .ok_or("could not determine repository root from the .taska directory")?;
    git::setup(repo_root)?;
    Ok(outcome)
}
