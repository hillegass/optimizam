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
    /// Stop once the best solution is proven this near the optimum.
    relative_gap: Option<f64>,
    absolute_gap: Option<f64>,
    threads: Option<usize>,
    explain: bool,
    constraints: bool,
    binding_only: bool,
    nonzeros: bool,
    /// Names for `Model::check` to write out.
    show: Vec<String>,
    /// A solve inside a search for a conflict: nothing is explained.
    trial: bool,
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

    /// How many threads a search may use. The default, also given by 0, is
    /// one per core of the machine, at most 8.
    ///
    /// With more than one, the CP engine runs a portfolio when it optimizes
    /// or looks for one solution (workers with different strategies that
    /// share the best solution and what they learn), and the MILP engine
    /// and the global nonlinear search share their search tree among
    /// workers. The answers are as valid as with one thread, but which
    /// solution is found, and how long it takes, then vary from run to run.
    /// Ask for one thread to have a solve repeat exactly, or when solving
    /// many models side by side on threads of your own. LPs, local nonlinear
    /// solves, and enumerating all solutions use one thread.
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = Some(threads);
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

    /// Stop once the best solution found is proven to be within `gap` of
    /// the optimum, as a share of its objective: when
    /// `|objective - bound| <= gap * max(1, |objective|)`, which is the
    /// [`gap`](crate::SolveResult::gap) results report. `0.01` settles for a
    /// solution within 1% of the best possible, often long before the
    /// search could prove the best one.
    ///
    /// The result then has status
    /// [`NotProvenOptimal`](crate::Status::NotProvenOptimal) (or
    /// `LocallyOptimal` for a nonlinear problem), the solution, the bound,
    /// and stop reason [`StopReason::Gap`](crate::StopReason::Gap). A search
    /// that finishes first gives its usual `Complete`: the engines call a
    /// solution optimal on their own once nothing can beat it by more than
    /// one part in a million (the global nonlinear search: in ten
    /// thousand), so a smaller gap than that changes nothing.
    ///
    /// For a minimization or maximization that asks for one solution;
    /// ignored otherwise. How soon it stops depends on the engine's bound,
    /// which is weak for scheduling problems solved by the CP engine.
    ///
    /// ```
    /// use optimizam::{Model, Options, StopReason, constraint, var};
    ///
    /// let mut m = Model::new();
    /// let x = m.add_vars(30, var().binary().name("x"));
    /// let weight = |i: usize| 20 + (i * 37 % 23) as i32;
    /// let value = |i: usize| weight(i) + (i * 11 % 7) as i32;
    /// m.add(constraint!((0..30).map(|i| weight(i) * x[i]).sum::<optimizam::Expression>() <= 401));
    /// m.maximize((0..30).map(|i| value(i) * x[i]).sum::<optimizam::Expression>());
    /// let result = m.solve(&Options::default().relative_gap(0.5))?;
    /// // Any solution at all is within half of the best possible here.
    /// if result.stop_reason() == Some(StopReason::Gap) {
    ///     assert!(result.gap().is_some_and(|gap| gap <= 0.5));
    /// }
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    ///
    /// # Panics
    ///
    /// If `gap` is negative or not a number.
    pub fn relative_gap(mut self, gap: f64) -> Self {
        assert!(gap >= 0.0, "a gap is a number of 0 or more, not {gap}");
        self.relative_gap = Some(gap);
        self
    }

    /// Stop once the best solution found is proven to be within `gap` of
    /// the optimum in the objective's own units: when
    /// `|objective - bound| <= gap`. Otherwise as
    /// [`relative_gap`](Options::relative_gap); with both set, the search
    /// stops at whichever is reached first.
    ///
    /// # Panics
    ///
    /// If `gap` is negative or not a number.
    pub fn absolute_gap(mut self, gap: f64) -> Self {
        assert!(gap >= 0.0, "a gap is a number of 0 or more, not {gap}");
        self.absolute_gap = Some(gap);
        self
    }

    /// The gaps to stop at: relative, absolute.
    pub(crate) fn gaps(&self) -> (Option<f64>, Option<f64>) {
        (self.relative_gap, self.absolute_gap)
    }

    /// When the problem turns out infeasible, go on to look for a small set
    /// of constraints that can't hold together, and report it as the
    /// result's [`conflict`](crate::SolveResult::conflict) (default false).
    ///
    /// The search removes the constraints one group at a time (the instances
    /// of an indexed constraint of a problem file together, then one by
    /// one), solving what is left each time, so it can take many times as
    /// long as the solve did. It stops at the time limit, which it shares
    /// with the solve; the conflict it then reports may not be minimal.
    /// [`Model::conflict`](crate::Model::conflict) runs the same search by
    /// itself.
    pub fn explain(mut self, on: bool) -> Self {
        self.explain = on;
        self
    }

    pub(crate) fn explains(&self) -> bool {
        self.explain
    }

    /// For [`Model::check`](crate::Model::check): also write out what
    /// `name` comes to, as [`Check::shown`](crate::Check::shown) then has
    /// it. The name of a constraint (the first members of its family, or the
    /// one member `capacity[north]`), of a variable (its first elements,
    /// with their bounds), or of both. Call it once for each name; a solve
    /// takes no notice.
    pub fn show(mut self, name: impl Into<String>) -> Self {
        self.show.push(name.into());
        self
    }

    pub(crate) fn shows(&self) -> &[String] {
        &self.show
    }

    /// Report the constraints with the result (default false): the slack
    /// each solution leaves in every comparison constraint
    /// ([`Solution::slack`](crate::Solution::slack)), and, for a linear
    /// program (continuous variables, linear constraints, an objective), the
    /// constraints' dual values and the variables' reduced costs
    /// ([`SolveResult::dual`](crate::SolveResult::dual),
    /// [`reduced_cost`](crate::SolveResult::reduced_cost)).
    ///
    /// A linear program is then solved without presolve, which substitutes
    /// variables out through rows that would be left without a dual; a large
    /// one can take longer.
    pub fn constraints(mut self, on: bool) -> Self {
        self.constraints = on;
        self
    }

    /// With [`constraints`](Options::constraints): result files
    /// ([`SolveResult::to_json`](crate::SolveResult::to_json)) list only the
    /// constraints a solution leaves no slack in, and say so
    /// (`"omitted": ["slack"]`). Default false. The result itself still has
    /// every slack.
    pub fn binding_only(mut self, on: bool) -> Self {
        self.binding_only = on;
        self
    }

    /// Result files leave the zero entries out of indexed variables (and of
    /// reduced costs), and say so (`"omitted": ["zeros"]`): of a 50 by 50
    /// assignment's 2,500 entries, 50 are left. Default false. The result
    /// itself still has every value.
    pub fn nonzeros(mut self, on: bool) -> Self {
        self.nonzeros = on;
        self
    }

    /// What result files leave out: zero entries, constraints with slack.
    pub(crate) fn omissions(&self) -> (bool, bool) {
        (self.nonzeros, self.binding_only)
    }

    pub(crate) fn reports_constraints(&self) -> bool {
        self.constraints && !self.trial
    }

    pub(crate) fn is_trial(&self) -> bool {
        self.trial
    }

    /// The options of the solves inside a search for a conflict, which ends
    /// at `deadline`: the same engine, threads, and cancel token, and no
    /// progress callback or gap (their solutions aren't the problem's).
    pub(crate) fn for_trials(&self, deadline: Option<Instant>) -> Options {
        Options {
            time_limit: None,
            deadline,
            progress: None,
            relative_gap: None,
            absolute_gap: None,
            explain: false,
            trial: true,
            ..self.clone()
        }
    }

    /// Whether `deadline` has passed or the cancel token is canceled.
    pub(crate) fn expired(&self, deadline: Option<Instant>) -> bool {
        deadline.is_some_and(|d| Instant::now() >= d) || self.cancel.as_ref().is_some_and(CancelToken::is_canceled)
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
            threads: crate::config::thread_count(self.threads.unwrap_or(0)),
            plan: false,
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
            .field("threads", &self.threads.unwrap_or(0))
            .field("explain", &self.explain)
            .field("constraints", &self.constraints)
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
