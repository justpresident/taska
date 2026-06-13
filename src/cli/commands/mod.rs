//! One module per `ta` subcommand handler.
//!
//! Each handler is thin: it parses/renders for the CLI and delegates the data
//! work to the frontend-agnostic [`crate::action`] layer. Handlers depend on the
//! [`EventStore`](crate::storage::EventStore) abstraction (not the concrete
//! `FileStore`, except the few that need git or filesystem paths) and reach the
//! shared CLI plumbing - `parse_field_ops`, `confirm`, `print_warnings` -
//! through the parent [`crate::cli`] module.

pub mod compact;
pub mod completions;
pub mod config;
pub mod create;
pub mod delete;
pub mod dep;
pub mod edit;
pub mod init;
pub mod list;
pub mod prime;
pub mod repair;
pub mod resolve;
pub mod show;
pub mod status;
pub mod undo;
pub mod update;

pub use compact::cmd_compact;
pub use completions::{cmd_completions, offer_install, InstallScope};
pub use config::{cmd_config, ConfigAction};
pub use create::cmd_create;
pub use delete::cmd_delete;
pub use dep::{cmd_dep_group, DepAction};
pub use edit::cmd_edit;
pub use init::cmd_init;
pub use list::cmd_list;
pub use prime::cmd_prime;
pub use repair::cmd_repair;
pub use resolve::cmd_resolve;
pub use show::cmd_show;
pub use status::cmd_status;
pub use undo::cmd_undo;
pub use update::cmd_update;
