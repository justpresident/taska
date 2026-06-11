//! `ta init` — provision the store, register the git merge drivers, and sync the
//! agent-integration block in CLAUDE.md / AGENTS.md.

use crate::action::init::{init, AgentFileStatus, StoreInit};
use crate::error::DynError;

pub fn cmd_init() -> Result<(), DynError> {
    let outcome = init()?;
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
    Ok(())
}
