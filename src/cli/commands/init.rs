//! `ta init` - provision the store, register the merge drivers (git or
//! mercurial), and sync the agent-integration block in CLAUDE.md / AGENTS.md.

use crate::action::init::{init, AgentFileStatus, StoreInit};
use crate::error::DynError;

pub fn cmd_init(no_commit: bool) -> Result<(), DynError> {
    let outcome = init(!no_commit)?;
    match &outcome.store {
        StoreInit::Reused(dir) => println!("taska store already present at {}", dir.display()),
        StoreInit::Created(dir) => println!("Initialized taska store at {}", dir.display()),
    }
    for file in &outcome.agent_files {
        let msg = match file.status {
            AgentFileStatus::Created => "Wrote taska integration to",
            AgentFileStatus::Updated => "Updated taska integration in",
            AgentFileStatus::Unchanged => "taska integration already current in",
        };
        println!("{msg} {}", file.path.display());
    }
    if let Some(hash) = &outcome.commit {
        println!("Committed taska store as {hash}");
    }
    // On an interactive terminal, offer to set up shell completion (skipped in
    // scripts/CI and when it's already installed).
    super::offer_install();
    Ok(())
}
