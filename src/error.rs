//! Shared error type for the binary.
//!
//! `taska` is a print-and-exit CLI, so it mostly has nothing to gain from typed,
//! matchable errors. A boxed trait object lets `?` propagate any underlying
//! error (`io`, `serde_json`, `&str`/`String` messages) through one return
//! type. Swap this for `anyhow::Error` or a `thiserror` enum if the program
//! ever needs context chains or to branch on error kinds.

pub type DynError = Box<dyn std::error::Error>;

/// Process exit code for a `--if` precondition that didn't hold.
///
/// The write was rejected because the guard failed - distinct from a general
/// error (1) so an agent can detect "my claim lost the race" without parsing
/// stderr. A follow-up folds every exit code into one enum (1 program error, 2
/// schema validation, 3 precondition) and audits all exit sites; for now this is
/// the one non-default code, carried by [`CodedError`].
pub const EXIT_PRECONDITION_FAILED: u8 = 3;

/// An error that fixes the process exit code (beyond the default 1).
///
/// `main` prints it like any error, then reads [`CodedError::code`] to set the
/// exit status. Construct via the intent-named helpers so call sites never
/// hardcode a raw number.
#[derive(Debug)]
pub struct CodedError {
    code: u8,
    message: String,
}

impl CodedError {
    /// A `--if` precondition was not met (exit [`EXIT_PRECONDITION_FAILED`]).
    #[must_use]
    pub fn precondition(message: impl Into<String>) -> DynError {
        Box::new(Self {
            code: EXIT_PRECONDITION_FAILED,
            message: message.into(),
        })
    }

    /// The exit code this error should terminate the process with.
    #[must_use]
    pub const fn code(&self) -> u8 {
        self.code
    }
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}
