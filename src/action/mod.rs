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
//! So far this module owns the READ pipeline ([`read`]) and the display actions
//! (`list`/`show`/`status`); the remaining command actions are migrating here
//! incrementally (see `commands-action-presentation-split`).

pub mod list;
pub mod read;
pub mod show;
pub mod status;

pub use list::{list_tasks, ListOutcome, ListQuery};
pub use read::{read, Session, Warning};
pub use show::{show, ShowOutcome};
pub use status::{status, StatusOutcome, StatusSummary};
