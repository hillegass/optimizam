//! Solver options.

use super::progress::{Control, Progress};
use crate::config::{Config, LogFn};
use crate::solve::Engine;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// let options = Options::default().time_limit(Duration::from_secs(10)).engine(Engine::Milp);
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
    cancel: Option<CancelToken>,
    progress: Option<ProgressFn>,
}

/// A progress callback, as [`Options::on_progress`] stores it.
pub(crate) type ProgressFn = Arc<dyn Fn(&Progress<'_>) -> Control + Send + Sync>;

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

    /// Stop when `token` is canceled, as if the time limit had been reached:
    /// the solve returns normally, with the best solution found so far (for
    /// example [`Status::NotProvenOptimal`](crate::Status::NotProvenOptimal)
    /// and its [`objective_bound`](crate::SolveResult::objective_bound)), or
    /// [`Status::Timeout`](crate::Status::Timeout) if it had found none.
    ///
    /// The options keep a clone of the token, so one token can stop several
    /// solves. See [`CancelToken`] for an example.
    pub fn cancel_on(mut self, token: &CancelToken) -> Self {
        self.cancel = Some(token.clone());
        self
    }

    /// Call `f` as the solve makes progress: with each better solution
    /// ([`Event::Solution`](crate::Event::Solution)), and at most about twice
    /// a second with a better bound or, failing that, a heartbeat. See
    /// [`Progress`] for what each call carries, and an example.
    ///
    /// `f` runs on the solving thread, in the middle of the search, so it
    /// should be quick (save the solution, send it on a channel, update a
    /// display). Returning [`Control::Stop`] stops the solve as a
    /// [`CancelToken`] would, with stop reason
    /// [`StopReason::Callback`](crate::StopReason::Callback).
    pub fn on_progress(mut self, f: impl Fn(&Progress<'_>) -> Control + Send + Sync + 'static) -> Self {
        self.progress = Some(Arc::new(f));
        self
    }

    pub(crate) fn progress_callback(&self) -> Option<ProgressFn> {
        self.progress.clone()
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
        Config {
            presolve: !self.no_presolve,
            symmetry: !self.no_symmetry,
            lns: !self.no_lns,
            log: self.log.clone(),
            cancel: self.cancel.as_ref().map(|t| t.0.clone()),
        }
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
            .field("cancel_on", &self.cancel)
            .field("on_progress", &self.progress.is_some())
            .finish()
    }
}

/// Stops solves from another thread (or from a signal handler, or a
/// timeout of your own), keeping what they have found.
///
/// Give it to a solve with [`Options::cancel_on`], then call
/// [`cancel`](CancelToken::cancel) from anywhere. The solve stops within
/// moments (the engines check it where they check the time limit) and
/// returns its result as if its time limit had been reached. Clones share
/// the same flag; a canceled token stays canceled, so make a new one for the
/// next solve.
///
/// ```
/// use optimizam::{CancelToken, Model, Options, constraint, var};
///
/// let mut m = Model::new();
/// let x = m.add_var(var().integer().bounds(0, 10));
/// m.add(constraint!(2 * x <= 7));
/// m.maximize(x);
///
/// let token = CancelToken::new();
/// let options = Options::default().cancel_on(&token);
/// let result = std::thread::scope(|s| {
///     let solve = s.spawn(|| m.solve(&options));
///     // ... later, perhaps on a user's request:
///     token.cancel();
///     solve.join().unwrap()
/// })?;
/// // Either finished before the cancel, or stopped with what it had.
/// println!("{:?}", result.status());
/// # Ok::<(), optimizam::Error>(())
/// ```
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that hasn't been canceled.
    pub fn new() -> CancelToken {
        CancelToken::default()
    }

    /// Stops every solve using this token (or a clone of it), now and later.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether [`cancel`](CancelToken::cancel) has been called.
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken").field("canceled", &self.is_canceled()).finish()
    }
}
