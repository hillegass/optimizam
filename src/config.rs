//! The settings of the solve running on this thread: whether presolve, value
//! symmetry, and large neighborhood search are on, where statistics go, and
//! whether the solve has been canceled ([`expired`]).
//!
//! The public entry points ([`crate::Model::solve`], [`crate::Problem::solve`])
//! install them for the duration of a solve with [`scoped`], so the engines
//! read them where they need them instead of having them threaded through
//! every signature. Solves on different threads don't see each other's.
//!
//! Worker threads that a solve spawns (the CP portfolio) start with nothing
//! installed: each installs the parent's [`Config`] ([`snapshot`]) with
//! [`scoped`], or it would silently run with the environment's defaults and
//! without the logger. Workers also share a flag ([`halting`]) that ends
//! every one's search when one of them has finished it.
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
    /// Threads a search may use (1: none of its own).
    pub threads: usize,
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
            threads: std::env::var("OPTIMIZAM_THREADS").ok().and_then(|v| v.parse().ok()).map_or(1, thread_count),
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
    /// The flags that stop this thread's searches: one for each parallel
    /// search it is working in, the innermost last (see [`halting`]).
    static HALT: RefCell<Vec<Arc<AtomicBool>>> = const { RefCell::new(Vec::new()) };
}

/// `threads` as a count: 0 stands for every core.
pub(crate) fn thread_count(threads: usize) -> usize {
    match threads {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    }
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
    halt: Vec<Arc<AtomicBool>>,
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
            HALT.with(|h| *h.borrow_mut() = saved.halt);
        }
    }
    let saved = Saved {
        cancel: CANCEL.with(|c| c.replace(config.cancel.clone())),
        config: CURRENT.with(|c| c.borrow_mut().replace(Rc::new(config))),
        stopped: STOPPED.replace(false),
        reason: REASON.replace(None),
        observer: OBSERVER.with(|o| o.borrow_mut().take()),
        halt: HALT.take(),
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

/// How many threads a search may use.
pub(crate) fn threads() -> usize {
    current().threads
}

/// The settings installed on this thread, for a worker thread to install.
pub(crate) fn snapshot() -> Config {
    (*current()).clone()
}

/// Runs `f` with `flag` among this thread's halt flags: while one of them is
/// set, [`expired`] is true, without a reason of this thread's own. The
/// workers of a parallel search share one, which the first to finish sets;
/// a search inside one of them (a sub-MILP, a batch of probes) adds its own
/// and still stops with the outer one.
pub(crate) fn halting<R>(flag: &Arc<AtomicBool>, f: impl FnOnce() -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            HALT.with(|h| h.borrow_mut().pop());
        }
    }
    HALT.with(|h| h.borrow_mut().push(flag.clone()));
    let _restore = Restore;
    f()
}

/// Runs a team of `threads` workers: `first` on this thread, and `others(1)`
/// to `others(threads - 1)` on threads of their own, which get this thread's
/// settings and halt flags (a new thread has neither: it would run with the
/// environment's defaults, without a logger, past every limit but its own
/// deadline). Returns what `first` returned, and the others in order.
///
/// The others have no observer and their stop reasons are their own, so the
/// search must report progress, and learn why it stopped, on this thread.
pub(crate) fn team<T, U: Send>(
    threads: usize,
    others: impl Fn(usize) -> U + Sync,
    first: impl FnOnce() -> T,
) -> (T, Vec<U>) {
    if threads <= 1 {
        return (first(), Vec::new());
    }
    let config = snapshot();
    let flags = HALT.with(|h| h.borrow().clone());
    let others = &others;
    std::thread::scope(|scope| {
        let spawned: Vec<_> = (1..threads)
            .map(|id| {
                let (config, flags) = (config.clone(), flags.clone());
                scope.spawn(move || {
                    scoped(config, || {
                        HALT.with(|h| *h.borrow_mut() = flags);
                        others(id)
                    })
                })
            })
            .collect();
        let first = first();
        (first, spawned.into_iter().map(|t| t.join().expect("a worker thread panicked")).collect())
    })
}

/// Whether the solve should stop: its deadline has passed, or it was
/// canceled. Every engine checks this where it checks its deadline, so a
/// canceled solve ends exactly like one that ran out of time (with the best
/// solution found so far), and sub-searches stop with it.
///
/// It also records why (see [`stop_reason`]), and gives the observer, if
/// there is one, a chance for a periodic event.
pub(crate) fn expired(deadline: Option<Instant>) -> bool {
    if HALT.with(|h| h.borrow().iter().any(|f| f.load(Ordering::Relaxed))) {
        return true;
    }
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
