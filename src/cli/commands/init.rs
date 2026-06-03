//! `ta init` — provision the store and register the git merge drivers.

use crate::error::DynError;
use crate::git;
use crate::storage::FileStore;

/// Idempotent: reuse an existing store if one is discoverable from the current
/// directory (e.g. a fresh clone), otherwise create one here. Either way, the
/// git merge driver is (re)registered, so re-running `ta init` is how a clone
/// installs the driver into its local config.
pub fn cmd_init() -> Result<(), DynError> {
    // Resolve the store directory: reuse an existing one (so re-running from
    // anywhere in the repo is idempotent), else create one in the current dir.
    let base_dir = if let Ok(existing) = FileStore::discover() {
        println!(
            "taska store already present at {}",
            existing.base_dir.display()
        );
        existing.base_dir
    } else {
        let dir = std::env::current_dir()?.join(".taska");
        println!("Initialized taska store at {}", dir.display());
        dir
    };

    // Provision honors the (possibly user-edited) config, creating any newly
    // configured log files — this is what makes re-running `ta init` the way to
    // apply a change to the `[store]` paths.
    let store = FileStore::provision(base_dir)?;
    let repo_root = store
        .repo_root()
        .ok_or("could not determine repository root from the .taska directory")?;
    git::setup(repo_root)?;
    Ok(())
}
