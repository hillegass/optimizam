//! Native propagators for `no_overlap` and `cumulative` (instead of their
//! linear encodings), with explanations for conflict analysis.
//!
//! A task is an interval [start, end) with end = start + duration, a demand,
//! and an optional presence literal; only present tasks constrain anything.
//! Each propagator reads the current bounds and pushes new ones; each pushed
//! bound is stored with its explanation (true literals that imply it), which
//! the solver keeps until it backtracks past it. Every rule also runs on the
//! mirrored tasks (time negated: start' = -end, end' = -start), so pushing a
//! start up there pulls an end down.
//!
//! - `cumulative`: timetabling. The compulsory part of a present task,
//!   [latest start, earliest end), uses its minimum demand for sure. Where the
//!   profile of compulsory parts exceeds the capacity there is a conflict;
//!   a task that would overload some time if it covered it (at its current
//!   earliest start) must start after that time. An optional task that can't
//!   be placed at all is absent. The explanation of "the task can't cover
//!   time t" is the tasks whose compulsory parts cover t, the capacity's upper
//!   bound, and the task's own bounds that make it cover t if it starts by t.
//! - `no_overlap`: two rules. Pairwise precedences: if a can't end by the
//!   time b starts, b ends by the time a starts (an optional task that fits
//!   neither way is absent). This alone is exact once the tasks are fixed,
//!   including zero-length tasks, which may touch others but not lie strictly
//!   inside them. Edge finding: for a set of tasks that all lie in [a, b]
//!   (with total duration P) and another task i, if i and the set don't fit
//!   between min(a, earliest start of i) and b, i ends last, so it starts no
//!   earlier than a + P. A set with a + P > b is a conflict on its own.

use super::domains::{Domains, Lit, Reason};
use std::collections::BTreeMap;

/// Edge finding is cubic in the number of tasks; above this many present
/// tasks, `no_overlap` uses the pairwise rule only.
const MAX_EDGE_FINDING_TASKS: usize = 200;

/// `coef * x + offset` for a variable x with coef 1 or -1, or a constant.
#[derive(Clone, Copy, Debug)]
pub struct Term {
    pub var: Option<u32>,
    pub coef: i64,
    pub offset: i64,
}

impl Term {
    pub fn constant(c: i64) -> Term {
        Term { var: None, coef: 0, offset: c }
    }

    pub fn of(var: usize, coef: i64, offset: i64) -> Term {
        debug_assert!(coef == 1 || coef == -1);
        Term { var: Some(var as u32), coef, offset }
    }

    pub fn lb(&self, dom: &Domains) -> i64 {
        match self.var {
            None => self.offset,
            Some(x) if self.coef > 0 => dom.lb(x as usize) + self.offset,
            Some(x) => self.offset - dom.ub(x as usize),
        }
    }

    pub fn ub(&self, dom: &Domains) -> i64 {
        match self.var {
            None => self.offset,
            Some(x) if self.coef > 0 => dom.ub(x as usize) + self.offset,
            Some(x) => self.offset - dom.lb(x as usize),
        }
    }

    /// The literal [term >= v], or None for a constant.
    pub fn ge(&self, v: i64) -> Option<Lit> {
        let x = self.var? as usize;
        Some(if self.coef > 0 { Lit::ge(x, v - self.offset) } else { Lit::le(x, self.offset - v) })
    }

    /// The literal [term <= v], or None for a constant.
    pub fn le(&self, v: i64) -> Option<Lit> {
        let x = self.var? as usize;
        Some(if self.coef > 0 { Lit::le(x, v - self.offset) } else { Lit::ge(x, self.offset - v) })
    }

    pub fn neg(self) -> Term {
        Term { var: self.var, coef: -self.coef, offset: -self.offset }
    }

    pub fn plus(self, c: i64) -> Term {
        Term { offset: self.offset + c, ..self }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Task {
    pub start: Term,
    pub end: Term,
    pub duration: Term,
    pub demand: Term,
    /// A binary variable; None if always present.
    pub presence: Option<u32>,
}

impl Task {
    fn mirrored(&self) -> Task {
        Task { start: self.end.neg(), end: self.start.neg(), ..*self }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Presence {
    Present,
    Absent,
    Optional,
}

fn presence(dom: &Domains, p: Option<u32>) -> Presence {
    match p {
        None => Presence::Present,
        Some(x) if dom.lb(x as usize) >= 1 => Presence::Present,
        Some(x) if dom.ub(x as usize) <= 0 => Presence::Absent,
        Some(_) => Presence::Optional,
    }
}

fn present_lit(p: Option<u32>) -> Option<Lit> {
    p.map(|x| Lit::ge(x as usize, 1))
}

pub enum Kind {
    NoOverlap,
    Cumulative {
        capacity: Term,
    },
    /// `all_different` over the items (`alldiff.rs`); the resource has no tasks.
    AllDifferent {
        items: Vec<Term>,
    },
}

pub struct Resource {
    pub tasks: Vec<Task>,
    pub kind: Kind,
}

/// Makes `lit` true, explained by `why` (stored in `reasons`). Err: the
/// conflict, if `lit` contradicts the current bounds.
fn set(dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>, lit: Lit, why: Vec<Lit>) -> Result<(), Vec<Lit>> {
    let id = reasons.len() as u32;
    match dom.set(lit, Reason::Stored(id)) {
        Ok(true) => {
            reasons.push(why);
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(()) => {
            let mut conflict = why;
            let v = lit.var();
            conflict.push(if lit.upper { Lit::ge(v, dom.lb(v)) } else { Lit::le(v, dom.ub(v)) });
            Err(conflict)
        }
    }
}

/// Makes `term >= v` true (a conflict for a constant below v).
pub(super) fn set_ge(
    dom: &mut Domains,
    reasons: &mut Vec<Vec<Lit>>,
    term: Term,
    v: i64,
    why: Vec<Lit>,
) -> Result<(), Vec<Lit>> {
    match term.ge(v) {
        Some(lit) => set(dom, reasons, lit, why),
        None if term.offset >= v => Ok(()),
        None => Err(why),
    }
}

/// Makes the task absent.
fn set_absent(dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>, p: u32, why: Vec<Lit>) -> Result<(), Vec<Lit>> {
    set(dom, reasons, Lit::le(p as usize, 0), why)
}

impl Resource {
    /// The variables whose bounds the propagator reads.
    pub fn vars(&self) -> Vec<u32> {
        let mut vars: Vec<u32> = Vec::new();
        for t in &self.tasks {
            vars.extend([t.start.var, t.end.var, t.duration.var, t.demand.var, t.presence].into_iter().flatten());
        }
        match &self.kind {
            Kind::Cumulative { capacity } => vars.extend(capacity.var),
            Kind::AllDifferent { items } => vars.extend(items.iter().filter_map(|t| t.var)),
            Kind::NoOverlap => {}
        }
        vars.sort_unstable();
        vars.dedup();
        vars
    }

    pub fn propagate(&self, dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>) -> Result<(), Vec<Lit>> {
        let mirrored: Vec<Task> = self.tasks.iter().map(Task::mirrored).collect();
        match &self.kind {
            Kind::Cumulative { capacity } => {
                timetable(&self.tasks, *capacity, dom, reasons)?;
                timetable(&mirrored, *capacity, dom, reasons)
            }
            Kind::NoOverlap => {
                precedences(&self.tasks, dom, reasons)?;
                detectable_precedences(&self.tasks, dom, reasons)?;
                detectable_precedences(&mirrored, dom, reasons)?;
                edge_finding(&self.tasks, dom, reasons)?;
                edge_finding(&mirrored, dom, reasons)
            }
            Kind::AllDifferent { items } => super::alldiff::propagate(items, dom, reasons),
        }
    }
}

/// A compulsory part: task `task` surely runs in [from, to) with demand `h`.
struct Part {
    task: usize,
    from: i64,
    to: i64,
    h: i64,
}

/// Why the compulsory parts of the tasks (other than `except`) cover t.
fn covering(tasks: &[Task], parts: &[Part], t: i64, except: Option<usize>) -> Vec<Lit> {
    let mut why = Vec::new();
    for p in parts.iter().filter(|p| p.from <= t && t < p.to && Some(p.task) != except) {
        let task = &tasks[p.task];
        why.extend(
            [task.start.le(t), task.end.ge(t + 1), task.demand.ge(p.h), present_lit(task.presence)]
                .into_iter()
                .flatten(),
        );
    }
    why
}

/// Timetabling: pushes the starts of the tasks (see the module comment).
fn timetable(tasks: &[Task], capacity: Term, dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>) -> Result<(), Vec<Lit>> {
    let mut parts = Vec::new();
    for (k, t) in tasks.iter().enumerate() {
        if presence(dom, t.presence) != Presence::Present {
            continue;
        }
        let (from, to, h) = (t.start.ub(dom), t.end.lb(dom), t.demand.lb(dom));
        if h > 0 && from < to {
            parts.push(Part { task: k, from, to, h });
        }
    }
    if parts.is_empty() {
        return Ok(());
    }
    // The profile: segments [from, to) of constant positive height.
    let mut delta: BTreeMap<i64, i64> = BTreeMap::new();
    for p in &parts {
        *delta.entry(p.from).or_default() += p.h;
        *delta.entry(p.to).or_default() -= p.h;
    }
    let mut profile: Vec<(i64, i64, i64)> = Vec::new();
    let mut height = 0;
    let points: Vec<(i64, i64)> = delta.into_iter().collect();
    for w in points.windows(2) {
        height += w[0].1;
        if height > 0 {
            profile.push((w[0].0, w[1].0, height));
        }
    }
    let cap = capacity.ub(dom);
    let cap_lit = capacity.le(cap);
    // The highest point: a conflict above the capacity, else a lower bound on it.
    let &(peak_at, _, peak) = profile.iter().max_by_key(|s| s.2).unwrap();
    if peak > cap {
        let mut why = covering(tasks, &parts, peak_at, None);
        why.extend(cap_lit);
        return Err(why);
    }
    if peak > capacity.lb(dom) {
        set_ge(dom, reasons, capacity, peak, covering(tasks, &parts, peak_at, None))?;
    }

    for (k, t) in tasks.iter().enumerate() {
        let state = presence(dom, t.presence);
        let (p, h) = (t.duration.lb(dom), t.demand.lb(dom));
        if state == Presence::Absent || p <= 0 || h <= 0 {
            continue;
        }
        let own = parts.iter().find(|q| q.task == k).map(|q| (q.from, q.to));
        let lst = t.start.ub(dom);
        // The latest time in [a, b) that the task would overload.
        let overload = |a: i64, b: i64| -> Option<i64> {
            profile.iter().rev().filter(|s| s.0 < b && s.1 > a).find_map(|&(from, to, height)| {
                let mine = own.is_some_and(|(f, e)| f <= from && to <= e);
                let others = height - if mine { h } else { 0 };
                (others + h > cap).then(|| to.min(b) - 1)
            })
        };
        let mut s = t.start.lb(dom);
        let mut chain: Vec<Lit> = Vec::new(); // for an optional task
        let mut first = true;
        while s <= lst {
            let Some(at) = overload(s, s + p) else { break };
            let mut why = covering(tasks, &parts, at, Some(k));
            why.extend(cap_lit);
            why.extend([t.duration.ge(p), t.demand.ge(h)].into_iter().flatten());
            if state == Presence::Present {
                // Starting by `at`, the task would cover it.
                why.extend(t.start.ge(at + 1 - p));
                why.extend(present_lit(t.presence));
                set_ge(dom, reasons, t.start, at + 1, why)?;
            } else {
                // Hypothetical: only the first step's start bound is a fact.
                if first {
                    why.extend(t.start.ge(at + 1 - p));
                }
                chain.extend(why);
            }
            first = false;
            s = at + 1;
        }
        if state == Presence::Optional && s > lst {
            chain.extend(t.start.le(lst));
            set_absent(dom, reasons, t.presence.unwrap(), chain)?;
        }
    }
    Ok(())
}

/// Pairwise precedences for `no_overlap` (see the module comment).
fn precedences(tasks: &[Task], dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>) -> Result<(), Vec<Lit>> {
    for i in 0..tasks.len() {
        for j in 0..tasks.len() {
            let (a, b) = (&tasks[i], &tasks[j]);
            let (pa, pb) = (presence(dom, a.presence), presence(dom, b.presence));
            if i == j || pa == Presence::Absent || pb == Presence::Absent {
                continue;
            }
            if pa == Presence::Optional && pb == Presence::Optional {
                continue;
            }
            // a can't end by the time b starts?
            let b_lst = b.start.ub(dom);
            if a.end.lb(dom) <= b_lst {
                continue;
            }
            if pa == Presence::Present
                && pb == Presence::Present
                && a.start.lb(dom) >= b.end.lb(dom)
                && b.end.ub(dom) <= a.start.ub(dom)
            {
                continue; // both pushes already hold
            }
            let base: Vec<Lit> = [a.end.ge(b_lst + 1), b.start.le(b_lst)].into_iter().flatten().collect();
            if pa == Presence::Present && pb == Presence::Present {
                // So b ends by the time a starts. (Explanations are built only
                // for bounds that change: at a fixed point, most don't.)
                let both = [present_lit(a.presence), present_lit(b.presence)];
                let b_ect = b.end.lb(dom);
                if a.start.lb(dom) < b_ect {
                    let mut why = base.clone();
                    why.extend(both.into_iter().flatten().chain(b.end.ge(b_ect)));
                    set_ge(dom, reasons, a.start, b_ect, why)?;
                }
                let a_lst = a.start.ub(dom);
                if b.end.ub(dom) > a_lst {
                    let mut why = base;
                    why.extend(both.into_iter().flatten().chain(a.start.le(a_lst)));
                    set_ge(dom, reasons, b.end.neg(), -a_lst, why)?;
                }
            } else {
                // One is optional: absent if b can't end by the time a starts either.
                let a_lst = a.start.ub(dom);
                if b.end.lb(dom) <= a_lst {
                    continue;
                }
                let mut why = base;
                why.extend([b.end.ge(a_lst + 1), a.start.le(a_lst)].into_iter().flatten());
                let (optional, other) = if pa == Presence::Optional { (a, b) } else { (b, a) };
                why.extend(present_lit(other.presence));
                set_absent(dom, reasons, optional.presence.unwrap(), why)?;
            }
        }
    }
    Ok(())
}

/// Edge finding for `no_overlap`: pushes starts (see the module comment).
/// Detectable precedences (Vilím): every present task j with
/// `ect_i > lst_j` must come before task i, so i starts no earlier than the
/// earliest completion of the set of them, `max over a: a + sum of the
/// durations of those with est >= a`. That subsumes the pairwise rule's
/// push for a single j. Explained by [i ends >= ect_i], and for each task j
/// of the attaining subset, [j starts <= lst_j], [j starts >= a], and its
/// duration.
fn detectable_precedences(tasks: &[Task], dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>) -> Result<(), Vec<Lit>> {
    // Snapshot (index, est, ect, lst, duration) of the present tasks.
    let snap: Vec<(usize, i64, i64, i64, i64)> = tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| presence(dom, t.presence) == Presence::Present)
        .map(|(k, t)| (k, t.start.lb(dom), t.end.lb(dom), t.start.ub(dom), t.duration.lb(dom).max(0)))
        .collect();
    if snap.len() < 3 {
        return Ok(()); // two tasks: the pairwise rule is exact
    }
    let mut by_est: Vec<usize> = (0..snap.len()).collect();
    by_est.sort_unstable_by_key(|&m| std::cmp::Reverse(snap[m].1));
    for (m, &(k, est, ect, _, _)) in snap.iter().enumerate() {
        // Sweep the predecessors by decreasing est: the best (a + total).
        let (mut total, mut best, mut best_a) = (0, i64::MIN, 0);
        let mut count = 0;
        for &o in &by_est {
            let (_, e, _, lst, p) = snap[o];
            if o == m || ect <= lst {
                continue;
            }
            total += p;
            count += 1;
            if e + total > best {
                (best, best_a) = (e + total, e);
            }
        }
        if count < 2 || best <= est {
            continue; // nothing beyond the pairwise rule
        }
        let t = &tasks[k];
        let mut why: Vec<Lit> = t.end.ge(ect).into_iter().collect();
        for &(o, e, _, lst, p) in &snap {
            if o == k || ect <= lst || e < best_a {
                continue;
            }
            let u = &tasks[o];
            why.extend([u.start.le(lst), u.start.ge(best_a), u.duration.ge(p)].into_iter().flatten());
        }
        set_ge(dom, reasons, t.start, best, why)?;
    }
    Ok(())
}

fn edge_finding(tasks: &[Task], dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>) -> Result<(), Vec<Lit>> {
    // Snapshot (est, lct, duration) of the present tasks. Bounds only tighten,
    // so literals built from the snapshot stay true.
    let snap: Vec<(usize, i64, i64, i64)> = tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| presence(dom, t.presence) == Presence::Present)
        .map(|(k, t)| (k, t.start.lb(dom), t.end.ub(dom), t.duration.lb(dom).max(0)))
        .collect();
    if snap.len() < 2 || snap.len() > MAX_EDGE_FINDING_TASKS {
        return Ok(());
    }
    // Why the tasks lying in [a, b] (except `except`) do, with their total duration.
    let window = |a: i64, b: i64, except: Option<usize>| -> (Vec<Lit>, i64) {
        let mut why = Vec::new();
        let mut total = 0;
        for &(k, est, lct, p) in &snap {
            if est >= a && lct <= b && Some(k) != except {
                let t = &tasks[k];
                why.extend(
                    [t.start.ge(a), t.end.le(b), t.duration.ge(p), present_lit(t.presence)].into_iter().flatten(),
                );
                total += p;
            }
        }
        (why, total)
    };
    let mut lcts: Vec<i64> = snap.iter().map(|s| s.2).collect();
    lcts.sort_unstable();
    lcts.dedup();
    // The tasks by decreasing earliest start, filtered for each b below.
    let mut by_est: Vec<&(usize, i64, i64, i64)> = snap.iter().collect();
    by_est.sort_unstable_by_key(|s| std::cmp::Reverse(s.1));
    // For one b: the windows [a, b], by decreasing a, with the total duration
    // of the tasks in them (increasing), and the suffix maxima of a + total.
    let mut windows: Vec<(i64, i64)> = Vec::with_capacity(snap.len());
    let mut suffix: Vec<(i64, usize)> = Vec::with_capacity(snap.len());
    // Best start bound found for each task (index into snap): (bound, a, b).
    let mut best: Vec<Option<(i64, i64, i64)>> = vec![None; snap.len()];
    for &b in &lcts {
        windows.clear();
        let mut total = 0;
        let mut inside = by_est.iter().filter(|s| s.2 <= b).peekable();
        while let Some(s) = inside.next() {
            total += s.3;
            let a = s.1;
            if inside.peek().is_some_and(|next| next.1 == a) {
                continue; // the window [a, b] includes every task with this est
            }
            if a + total > b {
                return Err(window(a, b, None).0);
            }
            windows.push((a, total));
        }
        if windows.is_empty() {
            continue;
        }
        suffix.clear();
        suffix.resize(windows.len(), (i64::MIN, 0));
        for k in (0..windows.len()).rev() {
            let here = (windows[k].0 + windows[k].1, k);
            // Ties go to the earliest window, as in the direct sweep (same explanations).
            suffix[k] = if k + 1 < windows.len() && suffix[k + 1].0 > here.0 { suffix[k + 1] } else { here };
        }
        // Task m is pushed by a window [a, b] without it if
        // min(a, est_m) + total + p_m > b, to a + total.
        for (m, &(_, est, lct, p)) in snap.iter().enumerate() {
            let mut found: Option<(i64, i64)> = None;
            // Windows starting after est_m: [0, split). Their condition is
            // est_m + total + p_m > b, which holds from some window on.
            let split = windows.partition_point(|w| w.0 > est);
            let from = windows[..split].partition_point(|w| est + w.1 + p <= b);
            for &(a, total) in &windows[from..split] {
                if found.is_none_or(|f| a + total > f.0) {
                    found = Some((a + total, a));
                }
            }
            // Windows starting at or before est_m contain m unless lct_m > b;
            // their condition is a + total + p_m > b: test the largest.
            if lct > b && split < windows.len() {
                let (value, k) = suffix[split];
                if value + p > b && found.is_none_or(|f| value > f.0) {
                    found = Some((value, windows[k].0));
                }
            }
            if let Some((bound, a)) = found
                && bound > est
                && best[m].is_none_or(|(old, _, _)| bound > old)
            {
                best[m] = Some((bound, a, b));
            }
        }
    }
    for (m, found) in best.into_iter().enumerate() {
        let Some((bound, a, b)) = found else { continue };
        let (k, est, _, p) = snap[m];
        let t = &tasks[k];
        let (mut why, total) = window(a, b, Some(k));
        debug_assert_eq!(a + total, bound);
        why.extend([t.start.ge(a.min(est)), t.duration.ge(p), present_lit(t.presence)].into_iter().flatten());
        set_ge(dom, reasons, t.start, bound, why)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_task(start_var: usize, duration: i64) -> Task {
        let start = Term::of(start_var, 1, 0);
        Task {
            start,
            end: start.plus(duration),
            duration: Term::constant(duration),
            demand: Term::constant(1),
            presence: None,
        }
    }

    #[test]
    fn edge_finding_pushes_past_a_set() {
        // a, b in [0, 4) (durations 2, 2); c (duration 1) starts in 0..10.
        // a and b fill [0, 4), so c must start at 4 or later.
        let mut dom = Domains::new(vec![0, 0, 0], vec![2, 2, 10], vec![None; 3]);
        let tasks = vec![fixed_task(0, 2), fixed_task(1, 2), fixed_task(2, 1)];
        let r = Resource { tasks, kind: Kind::NoOverlap };
        let mut reasons = Vec::new();
        r.propagate(&mut dom, &mut reasons).unwrap();
        assert_eq!(dom.lb(2), 4);
    }

    #[test]
    fn detectable_precedences_push_past_the_set() {
        // a, b start in 0..3 (duration 2): either order between them. c
        // (duration 5) starts in 0..20, so it can't end by 3: both come
        // before it, and together they end at 4 at the earliest (the pairwise
        // rule alone gives 2).
        let mut dom = Domains::new(vec![0, 0, 0], vec![3, 3, 20], vec![None; 3]);
        let tasks = vec![fixed_task(0, 2), fixed_task(1, 2), fixed_task(2, 5)];
        let mut reasons = Vec::new();
        detectable_precedences(&tasks, &mut dom, &mut reasons).unwrap();
        assert_eq!(dom.lb(2), 4);
        // Explained by c's earliest end and a's and b's bounds.
        assert!(reasons[0].contains(&Lit::le(0, 3)) && reasons[0].contains(&Lit::le(1, 3)), "{:?}", reasons[0]);
    }

    /// The bounds of the cubic edge finding this replaced: for each b, a
    /// and task, directly. (Conflicts as None.)
    fn edge_finding_bounds_reference(snap: &[(usize, i64, i64, i64)]) -> Option<Vec<Option<i64>>> {
        let mut lcts: Vec<i64> = snap.iter().map(|s| s.2).collect();
        lcts.sort_unstable();
        lcts.dedup();
        let mut best: Vec<Option<i64>> = vec![None; snap.len()];
        for &b in &lcts {
            let mut inside: Vec<&(usize, i64, i64, i64)> = snap.iter().filter(|s| s.2 <= b).collect();
            inside.sort_by_key(|s| std::cmp::Reverse(s.1));
            let mut total = 0;
            for (n, s) in inside.iter().enumerate() {
                total += s.3;
                let a = s.1;
                if inside.get(n + 1).is_some_and(|next| next.1 == a) {
                    continue;
                }
                if a + total > b {
                    return None;
                }
                for (m, &(_, est, lct, p)) in snap.iter().enumerate() {
                    if est >= a && lct <= b {
                        continue;
                    }
                    let bound = a + total;
                    if a.min(est) + total + p > b && bound > est && best[m].is_none_or(|old| bound > old) {
                        best[m] = Some(bound);
                    }
                }
            }
        }
        Some(best)
    }

    /// On random task sets, edge finding pushes exactly the bounds of the
    /// cubic reference (one pass: the reference doesn't iterate either).
    #[test]
    fn edge_finding_matches_the_cubic_reference() {
        let mut state = 0x853c_49e6_748f_ea9bu64;
        let mut rand = |n: i64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as i64
        };
        for case in 0..3000 {
            let n = 2 + rand(6) as usize;
            let durations: Vec<i64> = (0..n).map(|_| rand(5)).collect();
            let lb: Vec<i64> = (0..n).map(|_| rand(10)).collect();
            let ub: Vec<i64> = lb.iter().map(|l| l + rand(12)).collect();
            let tasks: Vec<Task> = (0..n).map(|k| fixed_task(k, durations[k])).collect();
            let mut dom = Domains::new(lb.clone(), ub.clone(), vec![None; n]);
            let snap: Vec<(usize, i64, i64, i64)> =
                (0..n).map(|k| (k, lb[k], ub[k] + durations[k], durations[k])).collect();
            let want = edge_finding_bounds_reference(&snap);
            let got = edge_finding(&tasks, &mut dom, &mut Vec::new());
            match want {
                None => assert!(got.is_err(), "case {case}: expected a conflict"),
                Some(bounds) => {
                    // A push may cross the upper bound: a conflict there too.
                    let crosses = (0..n).any(|k| bounds[k].is_some_and(|b| b > ub[k]));
                    if crosses {
                        assert!(got.is_err(), "case {case}: a push past the upper bound");
                        continue;
                    }
                    assert!(got.is_ok(), "case {case}");
                    for k in 0..n {
                        assert_eq!(dom.lb(k), bounds[k].unwrap_or(lb[k]).max(lb[k]), "case {case}, task {k}");
                    }
                }
            }
        }
    }

    #[test]
    fn timetable_finds_overloads() {
        // Two tasks of demand 2 and duration 3 with capacity 3, both forced
        // to cover time 2: a conflict.
        let mut dom = Domains::new(vec![0, 0], vec![2, 2], vec![None; 2]);
        let mut tasks = vec![fixed_task(0, 3), fixed_task(1, 3)];
        for t in &mut tasks {
            t.demand = Term::constant(2);
        }
        let r = Resource { tasks, kind: Kind::Cumulative { capacity: Term::constant(3) } };
        assert!(r.propagate(&mut dom, &mut Vec::new()).is_err());
    }
}
