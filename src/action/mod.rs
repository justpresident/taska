//! The frontend-agnostic **action layer**: every operation a frontend performs
//! on a store, returning typed data and warnings-*as-data* — never printing.
//!
//! This is the seam that lets more than one frontend (the bundled CLI, a TUI, a
//! library consumer) drive the same functionality and present it differently.
//! An action depends only on the [`EventStore`](crate::storage::EventStore)
//! trait plus the domain layers (engine/schema/graph/config); it knows nothing
//! of `cli` or `format`. Presentation — `println!`, color, prompts, tables —
//! stays entirely in the frontend, which consumes an action's typed result and
//! renders it however it likes.
//!
//! Every command drives through here: `read`, the display actions
//! (`list`/`show`/`status`), the `dep` group, `write::{create,update,delete}`,
//! the plan→apply pairs (`undo`/`resolve`), `compact`, `repair`, `config`, and
//! `init`. [`materialize`] is the shared raw-materialization primitive every
//! write/maintenance action uses (so it lives here, not in any one sibling).

use std::collections::HashMap;

use crate::config::Config;
use crate::engine::Engine;
use crate::model::{MutationEvent, TaskState};

pub mod compact;
pub mod config;
pub mod dep;
pub mod init;
pub mod list;
pub mod read;
pub mod repair;
pub mod resolve;
pub mod show;
pub mod status;
pub mod undo;
pub mod write;

pub use list::{list_tasks, ListOutcome, ListQuery};
pub use read::{read, Session, Warning};
pub use show::{show, ShowOutcome};
pub use status::{status, StatusOutcome, StatusSummary};

/// Materialize RAW state from baseline + log slices using `config`'s workflow.
///
/// The write-side counterpart to [`read`]: the `append_checked` verifier closures
/// and the maintenance actions hold slices read under the store lock (not a
/// store) and want raw state — canonical keys, no display shaping. A thin
/// convenience over [`Engine::materialize_state`], at the action root so no
/// action depends on a *sibling* module for it.
pub(crate) fn materialize(
    config: &Config,
    baseline: &[TaskState],
    log: &[MutationEvent],
) -> HashMap<String, TaskState> {
    Engine::materialize_state(
        baseline.to_vec(),
        log.to_vec(),
        &config.workflow.done_status,
    )
}
