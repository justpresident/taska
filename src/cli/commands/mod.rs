//! One module per `ta` subcommand handler.
//!
//! Each handler depends on the [`EventStore`](crate::storage::EventStore)
//! abstraction (not the concrete `FileStore`, except the few that need git or
//! filesystem paths), and reaches shared plumbing — `state_of`, `replay`,
//! `parse_fields`, `confirm` — through the parent [`crate::cli`] module.

pub mod compact;
pub mod config;
pub mod create;
pub mod delete;
pub mod dep;
pub mod init;
pub mod list;
pub mod ready;
pub mod resolve;
pub mod show;
pub mod status;
pub mod undo;
pub mod update;

pub use compact::cmd_compact;
pub use config::{cmd_config, ConfigAction};
pub use create::cmd_create;
pub use delete::cmd_delete;
pub use dep::{cmd_dep, cmd_dep_group, DepAction};
pub use init::cmd_init;
pub use list::cmd_list;
pub use ready::cmd_ready;
pub use resolve::cmd_resolve;
pub use show::cmd_show;
pub use status::cmd_status;
pub use undo::cmd_undo;
pub use update::cmd_update;
