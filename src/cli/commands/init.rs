//! `ta init` — provision the store and register the git merge drivers.

use crate::action::init::{init, InitOutcome};
use crate::error::DynError;

pub fn cmd_init() -> Result<(), DynError> {
    match init()? {
        InitOutcome::Reused(dir) => {
            println!("taska store already present at {}", dir.display());
        }
        InitOutcome::Created(dir) => println!("Initialized taska store at {}", dir.display()),
    }
    Ok(())
}
