//! The error type of the public API.

use std::fmt;

/// Why a problem couldn't be read or solved.
///
/// An infeasible problem or a time limit is not an error: those come back as
/// a [`Status`](crate::Status) in a [`SolveResult`](crate::SolveResult).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

/// The kind of an [`Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The problem file isn't valid JSON, or doesn't have the structure of
    /// the problem format.
    Parse,
    /// The problem is malformed: an unknown name, a bad expression, a
    /// constraint that can't be evaluated, a variable used with another
    /// model, and so on.
    Invalid,
    /// The problem is well formed but uses something the solver can't take,
    /// such as a nonlinear term with an engine that solves linear problems.
    Unsupported,
    /// A file couldn't be read.
    Io,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        // A constraint without a name is called by its place,
        // `constraints[3]`, which says "constraint" already.
        Error { kind, message: message.into().replace("constraint constraints[", "constraints[") }
    }

    /// An error from the model builder or an engine, whose messages start
    /// with "unsupported:" for what the solver can't take.
    pub(crate) fn from_solver(message: String) -> Error {
        let kind = if message.starts_with("unsupported:") { ErrorKind::Unsupported } else { ErrorKind::Invalid };
        Error::new(kind, message)
    }

    /// The kind of error.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// What went wrong, naming the constraint or variable involved.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
