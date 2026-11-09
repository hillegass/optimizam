//! The settings of the solve running on this thread: whether presolve, value
//! symmetry, and large neighborhood search are on, where statistics go, and
//! whether the solve has been canceled ([`expired`]).
//!
//! The public entry points ([`crate::Model::solve`], [`crate::Problem::solve`])
//! install them for the duration of a solve with [`scoped`], so the engines
//! read them where they need them instead of having them threaded through
//! every signature. The solver doesn't spawn threads, so a per-thread setting
//! is exact, and solves on different threads don't see each other's.
//!
//! If the solver ever spawns worker threads (a parallel portfolio, say),
//! they start with nothing installed: each worker must install a clone of
//! the parent's [`Config`] with [`scoped`], or it silently runs with the
//! environment's defaults and without the logger.
//!
//! When nothing is installed (the crate's own unit tests, which call the
//! engines directly), the `OPTIMIZAM_*` environment variables decide, as they
//! did before the library existed.

use crate::api::StopReason;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Receives one line of statistics (no trailing newline).
pub(crate) type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Config {
    pub presolve: bool,
    pub symmetry: bool,
    pub lns: bool,
    /// Statistics are produced only when this is set.
    pub log: Option<LogFn>,
    /// Set (from any thread) to stop the solve as if its deadline had passed.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Config {
    fn from_env() -> Config {
        let set = |name: &str| std::env::var_os(name).is_some();
        Config {
            presolve: !set("OPTIMIZAM_NO_PRESOLVE"),
            symmetry: !set("OPTIMIZAM_NO_SYMMETRY"),
            lns: !set("OPTIMIZAM_NO_LNS"),
            log: set("OPTIMIZAM_DEBUG").then(|| Arc::new(|line: &str| eprintln!("{line}")) as LogFn),
            cancel: None,
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Config>>> = const { RefCell::new(None) };
    /// The installed config's cancel flag, kept apart so that [`expired`],
    /// which the simplex calls every iteration, is a borrow and a load.
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    /// Set when a progress callback asks the solve to stop.
    static STOPPED: Cell<bool> = const { Cell::new(false) };
    /// Why the solve was cut short, the first time it was.
    static REASON: Cell<Option<StopReason>> = const { Cell::new(None) };
    /// Receives the solve's progress (installed by [`observe`]).
    static OBSERVER: RefCell<Option<Rc<dyn Observer>>> = const { RefCell::new(None) };
}

/// Receives what the solve finds as it goes, in the model's columns (the
/// public API turns it into progress events).
pub(crate) trait Observer {
    /// A better solution, with its objective (None for satisfy).
    fn solution(&self, x: &[f64], objective: Option<f64>);
    /// A bound on the optimum, in the problem's sense.
    fn bound(&self, bound: f64);
    /// Called at every limit check: a chance for a periodic event.
    fn tick(&self);
}

/// What an engine's top-level search reports as it goes, in that engine's
/// own columns and objective; `solve.rs` implements it to map them back to
/// the model's and pass them to the [`Observer`].
pub(crate) trait Report {
    /// A better solution (the engine's columns).
    fn solution(&self, x: &[f64]);
    /// A valid bound on the optimum (the engine's objective).
    fn bound(&self, _bound: f64) {}
}

/// What `scoped` saves and restores.
struct Saved {
    config: Option<Rc<Config>>,
    cancel: Option<Arc<AtomicBool>>,
    stopped: bool,
    reason: Option<StopReason>,
    observer: Option<Rc<dyn Observer>>,
}

/// Runs `f` with `config` installed on this thread, restoring the previous
/// one afterwards (also when `f` panics).
pub(crate) fn scoped<R>(config: Config, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Saved>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let saved = self.0.take().unwrap();
            CURRENT.with(|c| *c.borrow_mut() = saved.config);
            CANCEL.with(|c| *c.borrow_mut() = saved.cancel);
            STOPPED.set(saved.stopped);
            REASON.set(saved.reason);
            OBSERVER.with(|o| *o.borrow_mut() = saved.observer);
        }
    }
    let saved = Saved {
        cancel: CANCEL.with(|c| c.replace(config.cancel.clone())),
        config: CURRENT.with(|c| c.borrow_mut().replace(Rc::new(config))),
        stopped: STOPPED.replace(false),
        reason: REASON.replace(None),
        observer: OBSERVER.with(|o| o.borrow_mut().take()),
    };
    let _restore = Restore(Some(saved));
    f()
}

fn current() -> Rc<Config> {
    CURRENT.with(|c| c.borrow().clone()).unwrap_or_else(|| Rc::new(Config::from_env()))
}

/// Whether statistics are wanted.
pub(crate) fn debug() -> bool {
    current().log.is_some()
}

pub(crate) fn presolve() -> bool {
    current().presolve
}

pub(crate) fn symmetry() -> bool {
    current().symmetry
}

pub(crate) fn lns() -> bool {
    current().lns
}

/// Whether the solve should stop: its deadline has passed, or it was
/// canceled. Every engine checks this where it checks its deadline, so a
/// canceled solve ends exactly like one that ran out of time (with the best
/// solution found so far), and sub-searches stop with it.
///
/// It also records why (see [`stop_reason`]), and gives the observer, if
/// there is one, a chance for a periodic event.
pub(crate) fn expired(deadline: Option<Instant>) -> bool {
    let reason = if canceled() {
        StopReason::Canceled
    } else if STOPPED.get() {
        StopReason::Callback
    } else if deadline.is_some_and(|d| Instant::now() >= d) {
        StopReason::TimeLimit
    } else {
        if let Some(o) = observer() {
            o.tick();
        }
        return false;
    };
    stopped_by(reason);
    true
}

/// Whether the solve was canceled.
fn canceled() -> bool {
    CANCEL.with(|c| c.borrow().as_ref().is_some_and(|f| f.load(Ordering::Relaxed)))
}

/// Records that the search was cut short (by a limit that [`expired`]
/// doesn't check, such as a node limit), unless it already was.
pub(crate) fn stopped_by(reason: StopReason) {
    if REASON.get().is_none() {
        REASON.set(Some(reason));
    }
}

/// Why the search was cut short, if it was.
pub(crate) fn stop_reason() -> Option<StopReason> {
    REASON.get()
}

/// Asks the solve to stop at its next limit check (a progress callback's
/// `Control::Stop`).
pub(crate) fn stop() {
    STOPPED.set(true);
}

/// Installs `observer` for the rest of the enclosing [`scoped`] solve.
pub(crate) fn observe(observer: Rc<dyn Observer>) {
    OBSERVER.with(|o| *o.borrow_mut() = Some(observer));
}

/// The installed observer, if any. Engines report through it only from
/// their top-level search (via `solve.rs`, which maps their columns back to
/// the model's): sub-searches work in their own columns.
pub(crate) fn observer() -> Option<Rc<dyn Observer>> {
    OBSERVER.with(|o| o.borrow().clone())
}

/// Sends a line of statistics to the installed logger, if there is one.
pub(crate) fn log(args: fmt::Arguments<'_>) {
    if let Some(f) = &current().log {
        f(&args.to_string());
    }
}

/// `debug_log!("nodes {n}")`: like `eprintln!`, to the solve's logger.
macro_rules! debug_log {
    ($($arg:tt)*) => { $crate::config::log(format_args!($($arg)*)) };
}
pub(crate) use debug_log;
