//! The settings of the solve running on this thread: whether presolve, value
//! symmetry, and large neighborhood search are on, and where statistics go.
//!
//! The public entry points ([`crate::Model::solve`], [`crate::Problem::solve`])
//! install them for the duration of a solve with [`scoped`], so the engines
//! read them where they need them instead of having them threaded through
//! every signature. The solver doesn't spawn threads, so a per-thread setting
//! is exact, and solves on different threads don't see each other's.
//!
//! When nothing is installed (the crate's own unit tests, which call the
//! engines directly), the `OPTIMIZAM_*` environment variables decide, as they
//! did before the library existed.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

/// Receives one line of statistics (no trailing newline).
pub(crate) type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Config {
    pub presolve: bool,
    pub symmetry: bool,
    pub lns: bool,
    /// Statistics are produced only when this is set.
    pub log: Option<LogFn>,
}

impl Config {
    fn from_env() -> Config {
        let set = |name: &str| std::env::var_os(name).is_some();
        Config {
            presolve: !set("OPTIMIZAM_NO_PRESOLVE"),
            symmetry: !set("OPTIMIZAM_NO_SYMMETRY"),
            lns: !set("OPTIMIZAM_NO_LNS"),
            log: set("OPTIMIZAM_DEBUG").then(|| Arc::new(|line: &str| eprintln!("{line}")) as LogFn),
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Config>>> = const { RefCell::new(None) };
}

/// Runs `f` with `config` installed on this thread, restoring the previous
/// one afterwards (also when `f` panics).
pub(crate) fn scoped<R>(config: Config, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Rc<Config>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|c| c.borrow_mut().replace(Rc::new(config)));
    let _restore = Restore(previous);
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
