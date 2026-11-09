//! Progress callbacks and stop reasons.

use super::options::ProgressFn;
use super::solution::{Layout, Solution};
use crate::model::{LinearModel, Sense};
use crate::result::Status;
use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Why a search stopped before it finished
/// ([`SolveResult::stop_reason`](crate::SolveResult::stop_reason)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StopReason {
    /// The time limit or deadline ([`Options::time_limit`](crate::Options::time_limit)).
    TimeLimit,
    /// A [`CancelToken`](crate::CancelToken) was canceled.
    Canceled,
    /// A progress callback returned [`Control::Stop`].
    Callback,
    /// The global nonlinear search reached its node limit, which applies
    /// when there is no time limit.
    NodeLimit,
    /// The global nonlinear search stopped because its bound stopped
    /// improving.
    NoProgress,
}

/// What a progress callback tells the solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Control {
    /// Keep going.
    Continue,
    /// Stop now, keeping the best solution found, as a
    /// [`CancelToken`](crate::CancelToken) would.
    Stop,
}

/// Why a progress callback was called ([`Progress::event`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Event {
    /// A better solution was found ([`Progress::solution`]).
    Solution,
    /// The bound on the optimum improved ([`Progress::bound`]).
    Bound,
    /// Nothing new; sent about twice a second, so a callback can show the
    /// time or decide to stop.
    Heartbeat,
}

/// How a solve is going: passed to the callback of
/// [`Options::on_progress`](crate::Options::on_progress).
///
/// Solutions come as they are found, each better than the last (for
/// satisfaction problems: the first one). With every solution wanted
/// ([`Model::all_solutions`](crate::Model::all_solutions)), only the
/// improvements come here; the full set is in the result. The CP engine's
/// bounds come from propagation, not an LP: valid, but often weak (on a job
/// shop, about the busiest machine's load), so its gaps overstate the
/// distance to the optimum.
///
/// ```
/// use optimizam::{Control, Event, Model, Options, constraint, var};
/// use std::sync::{Arc, Mutex};
///
/// let mut m = Model::new();
/// let x = m.add_vars(8, var().integer().bounds(0, 5));
/// m.add(constraint!(x.iter().map(|&v| 3 * v).sum::<optimizam::Expression>() <= 40));
/// m.maximize(x.iter().enumerate().map(|(k, &v)| (k as f64 + 1.0) * v).sum::<optimizam::Expression>());
///
/// let found = Arc::new(Mutex::new(Vec::new()));
/// let log = found.clone();
/// let options = Options::default().on_progress(move |p| {
///     if p.event() == Event::Solution {
///         log.lock().unwrap().push(p.objective().unwrap());
///     }
///     // Good enough within 1% of optimal.
///     if p.gap().is_some_and(|g| g < 0.01) { Control::Stop } else { Control::Continue }
/// });
/// let result = m.solve(&options)?;
/// // The result's solution is the last one reported.
/// assert_eq!(found.lock().unwrap().last().copied(), result.objective());
/// # Ok::<(), optimizam::Error>(())
/// ```
#[derive(Debug)]
pub struct Progress<'a> {
    event: Event,
    elapsed: Duration,
    best: Option<&'a Solution>,
    bound: Option<f64>,
    layout: &'a Layout,
    single: bool,
}

impl Progress<'_> {
    /// Why the callback was called.
    pub fn event(&self) -> Event {
        self.event
    }

    /// The time since the solve started (including building the model).
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// The best solution found so far.
    pub fn solution(&self) -> Option<&Solution> {
        self.best
    }

    /// The best solution's objective (None for satisfaction problems, or
    /// before the first solution).
    pub fn objective(&self) -> Option<f64> {
        self.best.and_then(|s| s.objective())
    }

    /// The best bound proved so far on the optimum, in the problem's sense
    /// (a lower bound when minimizing).
    pub fn bound(&self) -> Option<f64> {
        self.bound
    }

    /// The relative gap between the best solution and the bound,
    /// `|objective - bound| / max(1, |objective|)`, when both are known.
    pub fn gap(&self) -> Option<f64> {
        let (objective, bound) = (self.objective()?, self.bound?);
        Some((objective - bound).abs() / objective.abs().max(1.0))
    }

    /// The best solution so far as a result file (`docs/result-format.md`),
    /// as if the solve stopped now: status `not_proven_optimal`, with the
    /// bound. None before the first solution, and for satisfaction and
    /// all-solutions problems, whose intermediate solutions aren't results
    /// of their own.
    pub fn to_json(&self) -> Option<String> {
        let best = self.best.filter(|_| self.single)?;
        Some(best.result_json(self.layout, Status::NotProvenOptimal, self.bound))
    }
}

/// How often bound and heartbeat events are sent at most.
const PERIOD: Duration = Duration::from_millis(500);

/// Turns what the engines report into progress events.
pub(crate) struct Observer {
    callback: ProgressFn,
    layout: Arc<Layout>,
    maximize: bool,
    /// Optimizing for one solution: intermediate results are meaningful.
    single: bool,
    start: Instant,
    state: RefCell<State>,
}

struct State {
    best: Option<Solution>,
    bound: Option<f64>,
    /// The bound in the last event.
    sent_bound: Option<f64>,
    last_event: Instant,
}

impl Observer {
    pub(crate) fn new(callback: ProgressFn, model: &LinearModel, layout: Arc<Layout>, start: Instant) -> Observer {
        Observer {
            callback,
            layout,
            maximize: model.sense == Sense::Maximize,
            single: model.sense != Sense::Satisfy && !model.want_all,
            start,
            state: RefCell::new(State { best: None, bound: None, sent_bound: None, last_event: start }),
        }
    }

    /// Calls the callback (with no borrow of the state held, so it may do
    /// anything) and acts on its answer.
    fn send(&self, event: Event) {
        let now = Instant::now();
        let (best, bound) = {
            let mut s = self.state.borrow_mut();
            s.last_event = now;
            s.sent_bound = s.bound;
            (s.best.clone(), s.bound)
        };
        let progress = Progress {
            event,
            elapsed: now - self.start,
            best: best.as_ref(),
            bound,
            layout: &self.layout,
            single: self.single,
        };
        if (self.callback)(&progress) == Control::Stop {
            crate::config::stop();
        }
    }
}

impl crate::config::Observer for Observer {
    fn solution(&self, x: &[f64], objective: Option<f64>) {
        self.state.borrow_mut().best = Some(self.layout.solution(x, objective));
        self.send(Event::Solution);
    }

    fn bound(&self, bound: f64) {
        if !bound.is_finite() {
            return;
        }
        let due = {
            let mut s = self.state.borrow_mut();
            // Every reported bound is valid, so keep the tightest.
            let tighter = match s.bound {
                Some(b) if self.maximize => b.min(bound),
                Some(b) => b.max(bound),
                None => bound,
            };
            s.bound = Some(tighter);
            s.sent_bound != s.bound && s.last_event.elapsed() >= PERIOD
        };
        if due {
            self.send(Event::Bound);
        }
    }

    fn tick(&self) {
        let event = {
            let s = self.state.borrow();
            if s.last_event.elapsed() < PERIOD {
                return;
            }
            if s.sent_bound != s.bound { Event::Bound } else { Event::Heartbeat }
        };
        self.send(event);
    }
}
