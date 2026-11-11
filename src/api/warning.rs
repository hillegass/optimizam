//! Warnings: what a problem says that it probably doesn't mean.

use std::fmt;

/// Something a problem file says that it probably doesn't mean
/// ([`Model::warnings`](crate::Model::warnings)), or that makes a model
/// infeasible on its face ([`Model::check`](crate::Model::check)). A warning
/// stops nothing. It prints as its message, and compares with one:
/// `warning == "parameter bonus is never used"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    kind: WarningKind,
    place: Option<String>,
    message: String,
}

/// What a [`Warning`] is about, to filter by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WarningKind {
    /// A `forall` matches nothing, so its constraint doesn't exist.
    EmptyForall,
    /// An index of a `sum`, a list, or a `forall` hides an outer index, a
    /// parameter, or a variable of the same name.
    HiddenName,
    /// Several constraints have one name.
    SharedName,
    /// Nothing uses a parameter.
    UnusedParameter,
    /// A variable is in no constraint and not in the objective.
    UnusedVariable,
    /// The problem is infeasible on its face: a variable with no value
    /// within its bounds, or a constraint that can't hold within its
    /// variables' bounds.
    Infeasible,
}

impl WarningKind {
    /// The kind as the command-line tool's `--progress` and `--check` output
    /// spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            WarningKind::EmptyForall => "empty_forall",
            WarningKind::HiddenName => "hidden_name",
            WarningKind::SharedName => "shared_name",
            WarningKind::UnusedParameter => "unused_parameter",
            WarningKind::UnusedVariable => "unused_variable",
            WarningKind::Infeasible => "infeasible",
        }
    }
}

impl Warning {
    pub(crate) fn new(kind: WarningKind, place: Option<String>, message: String) -> Warning {
        Warning { kind, place, message: super::error::tidy(message) }
    }

    /// What it is about.
    pub fn kind(&self) -> WarningKind {
        self.kind
    }

    /// Where in the problem file it is, as a JSON path counting from 0
    /// (`constraints[3]`, `parameters.bonus`), when it is in one place.
    pub fn place(&self) -> Option<&str> {
        self.place.as_deref()
    }

    /// The warning in words.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl PartialEq<str> for Warning {
    fn eq(&self, other: &str) -> bool {
        self.message == other
    }
}

impl PartialEq<&str> for Warning {
    fn eq(&self, other: &&str) -> bool {
        self.message == *other
    }
}

impl PartialEq<String> for Warning {
    fn eq(&self, other: &String) -> bool {
        self.message == *other
    }
}
