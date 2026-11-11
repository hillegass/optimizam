//! The error type of the public API.

use crate::problem::{Issue, Issues};
use std::fmt;

/// Why a problem couldn't be read or solved.
///
/// An infeasible problem or a time limit is not an error: those come back as
/// a [`Status`](crate::Status) in a [`SolveResult`](crate::SolveResult).
///
/// A problem file can have several things wrong with it, and one reading
/// finds them all: the error is the first, and [`errors`](Error::errors)
/// gives every one (printing the error prints each on a line of its own).
///
/// ```
/// use optimizam::Model;
///
/// let error = Model::from_json(r#"{
///     "version": "1.3",
///     "sets": { "T": { "from": 1, "to": 3 } },
///     "variables": [{ "name": "y", "type": "int", "index": ["T"] },
///                   { "name": "z", "type": "integer", "lower": "0" }],
///     "constraints": [{ "name": "step", "forall": "t in T", "expr": "z[t + 1] >= 1" }],
///     "solve": { "method": "satisfy" }
/// }"#).unwrap_err();
/// let messages: Vec<&str> = error.errors().map(|e| e.message()).collect();
/// assert_eq!(messages, [
///     r#"invalid problem file: variables[0].type (y): expected "continuous", "integer", or "binary", found "int""#,
///     r#"invalid problem file: variables[1].lower (z): expected a number or an expression, found "0" (write numbers without quotes)"#,
///     "constraint step[1]: z[2]: z is a scalar variable and takes no subscript",
/// ]);
/// assert_eq!((error.place(), error.name()), (Some("variables[0].type"), Some("y")));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    /// Where in a problem file, and in which named item of it.
    place: Option<String>,
    name: Option<String>,
    /// The other errors of the same reading, and how many more weren't kept.
    others: Vec<Error>,
    omitted: usize,
}

/// The most errors that one reading of a file reports.
const MOST: usize = 20;

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
        Error { kind, message: tidy(message.into()), place: None, name: None, others: Vec::new(), omitted: 0 }
    }

    /// What one reading of a problem file found wrong: the first as the
    /// error, the rest as its `others`.
    pub(crate) fn from_issues(issues: &Issues) -> Error {
        let one = |issue: &Issue| {
            let error = if issue.structural {
                Error::new(ErrorKind::Parse, &issue.text)
            } else {
                Error::from_solver(issue.text.clone())
            };
            Error { place: issue.place.clone(), name: issue.name.clone(), ..error }
        };
        let mut all = issues.0.iter().take(MOST).map(one);
        let first = all.next().unwrap_or_else(|| Error::new(ErrorKind::Invalid, "the problem is invalid"));
        Error { others: all.collect(), omitted: issues.0.len().saturating_sub(MOST), ..first }
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

    /// Where in a problem file the error is, as a path counting from 0:
    /// `variables[1].type`, `constraints[3]`, `solve.objective`. None for
    /// an error that isn't about a place in a file (a model built in Rust,
    /// a file that isn't JSON).
    pub fn place(&self) -> Option<&str> {
        self.place.as_deref()
    }

    /// The name of the variable, constraint, parameter, or set that the
    /// error is in, if it has one.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// This error and the others that the same reading of a problem file
    /// found, in the file's order: structural ones first, then each
    /// constraint's. At most 20 ([`omitted`](Error::omitted) counts the rest).
    pub fn errors(&self) -> impl Iterator<Item = &Error> {
        std::iter::once(self).chain(&self.others)
    }

    /// How many more errors there were than [`errors`](Error::errors) gives.
    pub fn omitted(&self) -> usize {
        self.omitted
    }
}

/// A message as shown: a constraint without a name is called by its place,
/// `constraints[3]`, which says "constraint" already.
pub(crate) fn tidy(message: String) -> String {
    message.replace("constraint constraints[", "constraints[")
}

/// Every error of the reading, one to a line.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        for other in &self.others {
            write!(f, "\n{}", other.message)?;
        }
        match self.omitted {
            0 => Ok(()),
            n => write!(f, "\n(and {n} more)"),
        }
    }
}

impl std::error::Error for Error {}
