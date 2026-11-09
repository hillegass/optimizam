//! Solver options.

use crate::config::{Config, LogFn};
use crate::solve::Engine;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How to solve: time limit, engine, and switches for parts of the search.
///
/// Built with chained methods from the defaults (no time limit, the `Auto`
/// engine, everything on, no statistics):
///
/// ```
/// use optimizam::{Engine, Options};
/// use std::time::Duration;
///
/// let options = Options::default().time_limit(Duration::from_secs(10)).engine(Engine::Mip);
/// ```
///
/// Each method returns the changed options, so keep the result:
/// `options = options.engine(Engine::Cp)` (the compiler warns otherwise).
///
/// Without a time limit, most problems are solved to completion, which can
/// take arbitrarily long for hard ones. (Global nonlinear searches stop after
/// a fixed number of nodes when there is no time limit.)
#[derive(Clone, Default)]
#[must_use = "`Options` methods return the changed options; use the result"]
pub struct Options {
    time_limit: Option<Duration>,
    deadline: Option<Instant>,
    engine: Engine,
    no_presolve: bool,
    no_symmetry: bool,
    no_lns: bool,
    log: Option<LogFn>,
}

impl Options {
    /// Stop after this long, counted from the start of the solve (including
    /// building the model). The result then says what was found so far, e.g.
    /// [`Status::NotProvenOptimal`](crate::Status::NotProvenOptimal).
    pub fn time_limit(mut self, limit: Duration) -> Self {
        self.time_limit = Some(limit);
        self
    }

    /// Stop at this instant. With a time limit too, whichever comes first.
    pub fn deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Which engine to use (default [`Engine::Auto`]).
    pub fn engine(mut self, engine: Engine) -> Self {
        self.engine = engine;
        self
    }

    /// Whether to presolve MILP and CP problems (default true). Turning it
    /// off is for comparisons, or to isolate a presolve bug.
    pub fn presolve(mut self, on: bool) -> Self {
        self.no_presolve = !on;
        self
    }

    /// Whether to break value symmetry (interchangeable values of variables
    /// linked by `all_different`, as in graph coloring) with precedence
    /// constraints (default true). Never used when every solution is wanted.
    pub fn value_symmetry(mut self, on: bool) -> Self {
        self.no_symmetry = !on;
        self
    }

    /// Whether the CP engine runs large neighborhood search when it
    /// optimizes (default true).
    pub fn large_neighborhood_search(mut self, on: bool) -> Self {
        self.no_lns = !on;
        self
    }

    /// Send search statistics (presolve, cuts, incumbents and when they were
    /// found, nodes, iterations) to `f`, one line per call. Statistics are
    /// for tuning and debugging; their wording isn't stable.
    pub fn log(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.log = Some(Arc::new(f));
        self
    }

    /// Print search statistics to stderr (as [`log`](Options::log) with
    /// `eprintln!`).
    pub fn log_to_stderr(self) -> Self {
        self.log(|line| eprintln!("{line}"))
    }

    pub(crate) fn engine_choice(&self) -> Engine {
        self.engine
    }

    /// The deadline of a solve that starts at `start`.
    pub(crate) fn deadline_from(&self, start: Instant) -> Option<Instant> {
        // A limit too large to represent is no limit.
        let from_limit = self.time_limit.and_then(|t| start.checked_add(t));
        match (from_limit, self.deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(crate) fn config(&self) -> Config {
        Config { presolve: !self.no_presolve, symmetry: !self.no_symmetry, lns: !self.no_lns, log: self.log.clone() }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("time_limit", &self.time_limit)
            .field("deadline", &self.deadline)
            .field("engine", &self.engine)
            .field("presolve", &!self.no_presolve)
            .field("value_symmetry", &!self.no_symmetry)
            .field("large_neighborhood_search", &!self.no_lns)
            .field("log", &self.log.is_some())
            .finish()
    }
}
