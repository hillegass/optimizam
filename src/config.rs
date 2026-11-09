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

use std::cell::RefCell;
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
}

/// Runs `f` with `config` installed on this thread, restoring the previous
/// one afterwards (also when `f` panics).
pub(crate) fn scoped<R>(config: Config, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Rc<Config>>, Option<Arc<AtomicBool>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let (config, cancel) = (self.0.take(), self.1.take());
            CURRENT.with(|c| *c.borrow_mut() = config);
            CANCEL.with(|c| *c.borrow_mut() = cancel);
        }
    }
    let cancel = CANCEL.with(|c| c.replace(config.cancel.clone()));
    let previous = CURRENT.with(|c| c.borrow_mut().replace(Rc::new(config)));
    let _restore = Restore(previous, cancel);
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
pub(crate) fn expired(deadline: Option<Instant>) -> bool {
    canceled() || deadline.is_some_and(|d| Instant::now() >= d)
}

/// Whether the solve was canceled.
pub(crate) fn canceled() -> bool {
    CANCEL.with(|c| c.borrow().as_ref().is_some_and(|f| f.load(Ordering::Relaxed)))
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
