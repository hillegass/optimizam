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
    Cumulative { capacity: Term },
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
fn set_ge(dom: &mut Domains, reasons: &mut Vec<Vec<Lit>>, term: Term, v: i64, why: Vec<Lit>) -> Result<(), Vec<Lit>> {
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
        if let Kind::Cumulative { capacity } = &self.kind {
            vars.extend(capacity.var);
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
                edge_finding(&self.tasks, dom, reasons)?;
                edge_finding(&mirrored, dom, reasons)
            }
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
            let base: Vec<Lit> = [a.end.ge(b_lst + 1), b.start.le(b_lst)].into_iter().flatten().collect();
            if pa == Presence::Present && pb == Presence::Present {
                // So b ends by the time a starts.
                let both = [present_lit(a.presence), present_lit(b.presence)];
                let b_ect = b.end.lb(dom);
                let mut why = base.clone();
                why.extend(both.into_iter().flatten().chain(b.end.ge(b_ect)));
                set_ge(dom, reasons, a.start, b_ect, why)?;
                let a_lst = a.start.ub(dom);
                let mut why = base;
                why.extend(both.into_iter().flatten().chain(a.start.le(a_lst)));
                set_ge(dom, reasons, b.end.neg(), -a_lst, why)?;
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
    // Best start bound found for each task (index into snap): (bound, a, b).
    let mut best: Vec<Option<(i64, i64, i64)>> = vec![None; snap.len()];
    for &b in &lcts {
        let mut inside: Vec<&(usize, i64, i64, i64)> = snap.iter().filter(|s| s.2 <= b).collect();
        inside.sort_unstable_by_key(|s| std::cmp::Reverse(s.1));
        let mut total = 0;
        for (n, s) in inside.iter().enumerate() {
            total += s.3;
            let a = s.1;
            if inside.get(n + 1).is_some_and(|next| next.1 == a) {
                continue; // the window [a, b] includes every task with this est
            }
            if a + total > b {
                return Err(window(a, b, None).0);
            }
            for (m, &(_, est, lct, p)) in snap.iter().enumerate() {
                if est >= a && lct <= b {
                    continue; // in the set
                }
                let bound = a + total;
                if a.min(est) + total + p > b && bound > est && best[m].is_none_or(|(old, _, _)| bound > old) {
                    best[m] = Some((bound, a, b));
                }
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
