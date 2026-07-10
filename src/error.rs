//! Shared error type for the binary.
//!
//! `taska` is a print-and-exit CLI, so it mostly has nothing to gain from typed,
//! matchable errors. A boxed trait object lets `?` propagate any underlying
//! error (`io`, `serde_json`, `&str`/`String` messages) through one return
//! type. Swap this for `anyhow::Error` or a `thiserror` enum if the program
//! ever needs context chains or to branch on error kinds.

pub type DynError = Box<dyn std::error::Error>;

/// The process exit-code taxonomy, so an agent can branch on the *kind* of
/// failure without parsing stderr.
///
/// `main` maps a returned error to one of these: a plain error is
/// [`ExitCode::Error`]; a [`CodedError`] carries a specific code. (`ta watch`
/// separately exits `1` on a no-match timeout - a direct exit, not an error.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// General program error: bad input, a missing task, I/O, a bug. The default
    /// for any error not specifically categorized below.
    Error = 1,
    /// The write was rejected by the SCHEMA: a `[task_types]` conformance
    /// violation, or the soft-schema typo guard (an undeclared field name).
    Schema = 2,
    /// A conditional write's `--if` precondition was not met.
    Precondition = 3,
}

impl ExitCode {
    /// The numeric code, for `std::process::ExitCode::from`.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// An error that fixes the process exit code (beyond the default [`ExitCode::Error`]).
///
/// `main` prints it like any error, then reads [`CodedError::code`] for the exit
/// status. Construct via the intent-named helpers so call sites never hardcode a
/// code.
#[derive(Debug)]
pub struct CodedError {
    code: ExitCode,
    message: String,
}

impl CodedError {
    /// The write violated the declared schema - a `[task_types]` conformance
    /// failure or the soft-schema typo guard (exit [`ExitCode::Schema`]).
    #[must_use]
    pub fn schema(message: impl Into<String>) -> DynError {
        Box::new(Self {
            code: ExitCode::Schema,
            message: message.into(),
        })
    }

    /// A `--if` precondition was not met (exit [`ExitCode::Precondition`]).
    #[must_use]
    pub fn precondition(message: impl Into<String>) -> DynError {
        Box::new(Self {
            code: ExitCode::Precondition,
            message: message.into(),
        })
    }

    /// The exit code this error should terminate the process with.
    #[must_use]
    pub const fn code(&self) -> ExitCode {
        self.code
    }
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;

    #[test]
    fn coded_errors_carry_their_exit_code_through_a_downcast() {
        assert_eq!(ExitCode::Error.as_u8(), 1);
        assert_eq!(ExitCode::Schema.as_u8(), 2);
        assert_eq!(ExitCode::Precondition.as_u8(), 3);

        // The path `main` uses: downcast a boxed error to recover its code.
        let schema: DynError = CodedError::schema("bad fields");
        assert_eq!(
            schema.downcast_ref::<CodedError>().unwrap().code(),
            ExitCode::Schema
        );
        let precondition: DynError = CodedError::precondition("guard failed");
        assert_eq!(
            precondition.downcast_ref::<CodedError>().unwrap().code(),
            ExitCode::Precondition
        );
        // A plain error isn't a CodedError, so `main` falls back to 1.
        let plain: DynError = "just a message".into();
        assert!(plain.downcast_ref::<CodedError>().is_none());
    }
}
