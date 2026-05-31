//! Shared error type for the binary.
//!
//! `taska` is a print-and-exit CLI, so it has nothing to gain from typed,
//! matchable errors. A boxed trait object lets `?` propagate any underlying
//! error (`io`, `serde_json`, `&str`/`String` messages) through one return
//! type. Swap this for `anyhow::Error` or a `thiserror` enum if the program
//! ever needs context chains or to branch on error kinds.

pub type DynError = Box<dyn std::error::Error>;
