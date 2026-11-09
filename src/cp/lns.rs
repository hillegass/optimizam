//! Large neighborhood search: improving the incumbent by re-solving part of
//! the problem with the rest restricted, as OR-Tools CP-SAT does.
//!
//! Each round picks a neighborhood, builds a fresh solver for the model with
//! `objective <= best - 1` and the neighborhood's restrictions, starts it from
//! the incumbent's values, and searches it with a conflict limit. Every
//! restriction holds for the incumbent, so the sub-problem only lacks a
//! better solution when the neighborhood has none.
//!
//! With scheduling constraints, a neighborhood relaxes some tasks (by start
//! variable, so a game's two intervals go together) and keeps the others in
//! their incumbent order rather than at their incumbent times, so they can
//! still shift: on a `no_overlap`, a chain `end <= start` through its kept
//! tasks in start order; on a `cumulative`, the kept tasks are split into
//! unit-capacity lanes (greedy interval partitioning, a task of demand d
//! taking d lanes), with a chain through each lane, which keeps the kept
//! tasks' usage within the incumbent's capacity. The relaxed tasks are chosen
//! at random, as a run of tasks in incumbent start order (a time window), or
//! as all the tasks of random resources. Without scheduling constraints, a
//! neighborhood fixes a random subset of the columns to their incumbent
//! values.
//!
//! Each kind of neighborhood keeps its own relaxed fraction: it grows when
//! sub-searches finish within the limit and shrinks when they don't.

use super::IntModel;
use super::scheduling::Term;
use super::solver::Search;
use std::collections::BTreeMap;
use std::time::Instant;

/// Conflicts per sub-search.
pub const CONFLICTS: u64 = 100;
/// Above this capacity, a `cumulative`'s kept tasks keep their start times
/// instead of being split into lanes.
const MAX_LANES: i64 = 1000;
/// The least a round counts for, in conflicts.
const MIN_COST: u64 = 25;
const MIN_FRACTION: f64 = 0.02;
const MAX_FRACTION: f64 = 0.9;

#[derive(Clone, Copy, Debug)]
enum Kind {
    RandomTasks,
    TimeWindow,
    Resources,
    RandomColumns,
}

pub struct Lns {
    /// Each kind with its relaxed fraction, rounds, and improvements.
    kinds: Vec<(Kind, f64, u64, u64)>,
    next: usize,
    rng: u64,
    /// Distinct task start variables (the units relaxed).
    starts: Vec<u32>,
    pub rounds: u64,
    pub improvements: u64,
    pub conflicts: u64,
    pub seconds: f64,
}

/// An improvement found by a sub-search.
pub struct Improvement {
    pub value: i64,
    pub x: Vec<i64>,
}

impl Lns {
    pub fn new(m: &IntModel) -> Self {
        let mut starts: Vec<u32> = m.resources.iter().flat_map(|r| r.0.iter().filter_map(|t| t.start.var)).collect();
        starts.sort_unstable();
        starts.dedup();
        let kinds = if starts.is_empty() {
            vec![(Kind::RandomColumns, 0.3, 0, 0)]
        } else {
            vec![(Kind::RandomTasks, 0.2, 0, 0), (Kind::TimeWindow, 0.2, 0, 0), (Kind::Resources, 0.2, 0, 0)]
        };
        Lns { kinds, next: 0, rng: 0x2545f4914f6cdd1d, starts, rounds: 0, improvements: 0, conflicts: 0, seconds: 0.0 }
    }

    fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn below(&mut self, n: usize) -> usize {
        (self.rand() % n.max(1) as u64) as usize
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }

    /// One round on the incumbent `x` (with objective `best`); returns the
    /// best improvement it found, if any.
    pub fn round(
        &mut self,
        m: &IntModel,
        x: &[i64],
        best: i64,
        deadline: Option<Instant>,
        conflicts: u64,
    ) -> Option<Improvement> {
        self.rounds += 1;
        let k = self.next;
        self.next = (self.next + 1) % self.kinds.len();
        let (kind, fraction, ..) = self.kinds[k];
        let mut fixed: Vec<(u32, i64)> = Vec::new();
        let mut rows: Vec<(Vec<(u32, i64)>, i64)> = Vec::new();
        match kind {
            Kind::RandomColumns => {
                let mut cols: Vec<u32> = (0..m.num_cols as u32).collect();
                self.shuffle(&mut cols);
                let keep = ((1.0 - fraction) * cols.len() as f64).round() as usize;
                fixed.extend(cols[..keep].iter().map(|&j| (j, x[j as usize])));
            }
            _ => {
                let relaxed = self.relaxed_tasks(kind, fraction, m, x);
                self.keep_order(m, x, &relaxed, &mut fixed, &mut rows);
            }
        }
        debug_assert!(
            rows.iter().all(|(t, rhs)| t.iter().map(|&(j, a)| a * x[j as usize]).sum::<i64>() <= *rhs),
            "a restriction excludes the incumbent"
        );
        let started = Instant::now();
        let (mut s, obj) = super::solver_for(m, Some(best - 1), &rows, &fixed);
        let obj = obj.expect("an objective");
        s.set_phase(x);
        s.propagate_root();
        let mut found: Option<Improvement> = None;
        let start_conflicts = s.stats.conflicts;
        let complete = loop {
            let left = conflicts.saturating_sub(s.stats.conflicts - start_conflicts);
            if left == 0 {
                break false;
            }
            match s.search(deadline, Some(left)) {
                Search::Solution => {
                    let y = s.values();
                    let value = super::objective_value(m, &y);
                    s.lin.cons[obj as usize].rhs = value - 1;
                    s.restart();
                    s.requeue(obj);
                    s.propagate_root();
                    found = Some(Improvement { value, x: y });
                }
                Search::Exhausted => break true,
                Search::TimeLimit | Search::ConflictLimit => break false,
            }
        };
        // Building and propagating cost something too: a round counts as at
        // least MIN_COST conflicts, so the LNS phases end even when every
        // sub-problem fails at the root (as at a proven optimum).
        self.conflicts += (s.stats.conflicts - start_conflicts).max(MIN_COST);
        self.seconds += started.elapsed().as_secs_f64();
        let (_, f, rounds, improvements) = &mut self.kinds[k];
        *f = if complete { (*f * 1.25).min(MAX_FRACTION) } else { (*f / 1.25).max(MIN_FRACTION) };
        *rounds += 1;
        if found.is_some() {
            self.improvements += 1;
            *improvements += 1;
        }
        found
    }

    /// Statistics of each kind of neighborhood, for debugging.
    pub fn summary(&self) -> String {
        let kinds: Vec<String> = self.kinds.iter().map(|(k, f, r, i)| format!("{k:?} {i}/{r} at {f:.2}")).collect();
        kinds.join(", ")
    }

    /// Start variables to relax.
    fn relaxed_tasks(&mut self, kind: Kind, fraction: f64, m: &IntModel, x: &[i64]) -> Vec<bool> {
        let n = self.starts.len();
        let count = ((fraction * n as f64).round() as usize).clamp(1, n);
        let mut relaxed = vec![false; m.lb.len()];
        match kind {
            Kind::RandomTasks => {
                let mut v = self.starts.clone();
                self.shuffle(&mut v);
                v[..count].iter().for_each(|&s| relaxed[s as usize] = true);
            }
            Kind::TimeWindow => {
                let mut v = self.starts.clone();
                v.sort_by_key(|&s| (x[s as usize], s));
                let first = self.below(n - count + 1);
                v[first..first + count].iter().for_each(|&s| relaxed[s as usize] = true);
            }
            Kind::Resources => {
                let mut order: Vec<usize> = (0..m.resources.len()).collect();
                self.shuffle(&mut order);
                let mut done = 0;
                for r in order {
                    for t in &m.resources[r].0 {
                        if let Some(s) = t.start.var
                            && !relaxed[s as usize]
                        {
                            relaxed[s as usize] = true;
                            done += 1;
                        }
                    }
                    if done >= count {
                        break;
                    }
                }
            }
            Kind::RandomColumns => unreachable!(),
        }
        relaxed
    }

    /// Restrictions keeping the tasks that aren't relaxed in their incumbent
    /// order (and presence).
    fn keep_order(
        &self,
        m: &IntModel,
        x: &[i64],
        relaxed: &[bool],
        fixed: &mut Vec<(u32, i64)>,
        rows: &mut Vec<(Vec<(u32, i64)>, i64)>,
    ) {
        let value = |t: &Term| t.var.map_or(t.offset, |v| t.coef * x[v as usize] + t.offset);
        for (tasks, capacity) in &m.resources {
            let mut kept: Vec<usize> = Vec::new();
            for (i, t) in tasks.iter().enumerate() {
                if t.start.var.is_some_and(|s| relaxed[s as usize]) {
                    continue;
                }
                if let Some(p) = t.presence {
                    fixed.push((p, x[p as usize]));
                    if x[p as usize] == 0 {
                        continue;
                    }
                }
                kept.push(i);
            }
            kept.sort_by_key(|&i| (value(&tasks[i].start), value(&tasks[i].end), i));
            match capacity {
                None => {
                    for w in kept.windows(2) {
                        rows.extend(precedence(tasks[w[0]].end, tasks[w[1]].start));
                    }
                }
                Some(c) if value(c) > MAX_LANES => {
                    for &i in &kept {
                        if let Some(v) = tasks[i].start.var {
                            fixed.push((v, x[v as usize]));
                        }
                    }
                }
                Some(c) => {
                    // Lanes: the last task in each, by its end.
                    let cap = value(c).max(0) as usize;
                    let mut lanes: Vec<Option<usize>> = vec![None; cap];
                    for &i in &kept {
                        let d = value(&tasks[i].demand);
                        if d <= 0 {
                            continue;
                        }
                        let start = value(&tasks[i].start);
                        let free: Vec<usize> = (0..cap)
                            .filter(|&l| lanes[l].is_none_or(|j| value(&tasks[j].end) <= start))
                            .take(d as usize)
                            .collect();
                        if free.len() < d as usize {
                            // Not a feasible profile (can't happen): fix the start instead.
                            if let Some(v) = tasks[i].start.var {
                                fixed.push((v, x[v as usize]));
                            }
                            continue;
                        }
                        for l in free {
                            if let Some(j) = lanes[l] {
                                rows.extend(precedence(tasks[j].end, tasks[i].start));
                            }
                            lanes[l] = Some(i);
                        }
                    }
                }
            }
        }
    }
}

/// The row `a <= b` (None if it is constant).
fn precedence(a: Term, b: Term) -> Option<(Vec<(u32, i64)>, i64)> {
    let mut terms: BTreeMap<u32, i64> = BTreeMap::new();
    if let Some(v) = a.var {
        *terms.entry(v).or_default() += a.coef;
    }
    if let Some(v) = b.var {
        *terms.entry(v).or_default() -= b.coef;
    }
    let terms: Vec<(u32, i64)> = terms.into_iter().filter(|t| t.1 != 0).collect();
    (!terms.is_empty()).then_some((terms, b.offset - a.offset))
}

#[cfg(test)]
mod tests {
    use super::super::scheduling::Task;
    use super::super::{objective_value, solver_for};
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn int(&mut self, lo: i64, hi: i64) -> i64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            lo + (self.0 % (hi - lo + 1) as u64) as i64
        }
    }

    /// A random job shop (jobs x machines, each job visiting the machines in
    /// a random order) minimizing the makespan, with a cumulative of capacity
    /// 2 over all operations with demands 1 or 2 (to exercise the lanes).
    fn job_shop(rng: &mut Rng, jobs: usize, machines: usize) -> IntModel {
        let n = jobs * machines;
        let dur: Vec<i64> = (0..n).map(|_| rng.int(1, 9)).collect();
        let horizon: i64 = dur.iter().sum();
        let makespan = n as u32;
        let mut rows = Vec::new();
        let mut on_machine: Vec<Vec<Task>> = vec![Vec::new(); machines];
        let mut all = Vec::new();
        for j in 0..jobs {
            let mut order: Vec<usize> = (0..machines).collect();
            for i in (1..machines).rev() {
                order.swap(i, rng.int(0, i as i64) as usize);
            }
            for k in 0..machines {
                let v = j * machines + k;
                let task = Task {
                    start: Term::of(v, 1, 0),
                    end: Term::of(v, 1, dur[v]),
                    duration: Term::constant(dur[v]),
                    demand: Term::constant(rng.int(1, 2)),
                    presence: None,
                };
                on_machine[order[k]].push(task);
                all.push(task);
                if k > 0 {
                    rows.push((vec![(v as u32 - 1, 1), (v as u32, -1)], -dur[v - 1])); // after the previous step
                }
            }
            let last = (j * machines + machines - 1) as u32;
            rows.push((vec![(last, 1), (makespan, -1)], -dur[last as usize]));
        }
        let mut resources: Vec<(Vec<Task>, Option<Term>)> = on_machine.into_iter().map(|t| (t, None)).collect();
        resources.push((all, Some(Term::constant(2))));
        IntModel {
            lb: vec![0; n + 1],
            ub: vec![horizon; n + 1],
            ranges: vec![None; n + 1],
            rows,
            objective: Some(vec![(makespan, 1)]),
            resources,
            all_different: Vec::new(),
            num_cols: n + 1,
            infeasible: false,
        }
    }

    fn feasible(m: &IntModel, x: &[i64]) -> bool {
        let value = |t: &Term| t.var.map_or(t.offset, |v| t.coef * x[v as usize] + t.offset);
        let rows = m.rows.iter().all(|(t, rhs)| t.iter().map(|&(j, a)| a * x[j as usize]).sum::<i64>() <= *rhs);
        let resources = m.resources.iter().all(|(tasks, capacity)| {
            let cap = capacity.as_ref().map_or(1, value);
            tasks.iter().all(|a| {
                let t = value(&a.start);
                let demand = |b: &Task| if capacity.is_some() { value(&b.demand) } else { 1 };
                let used: i64 = tasks.iter().filter(|b| value(&b.start) <= t && t < value(&b.end)).map(demand).sum();
                used <= cap
            })
        });
        rows && resources
    }

    /// Every improvement LNS reports is feasible, strictly better, and no
    /// better than the optimum (found by exhaustive search).
    #[test]
    fn improvements_are_feasible() {
        let mut rng = Rng(0x853c49e6748fea9b);
        let mut improvements = 0;
        for case in 0..20 {
            let m = job_shop(&mut rng, 4, 3);
            let (mut s, obj) = solver_for(&m, Some(i64::MAX / 4), &[], &[]);
            let obj = obj.unwrap();
            assert!(matches!(s.search(None, None), Search::Solution));
            let mut x = s.values();
            let mut best = objective_value(&m, &x);
            let first = (best, x.clone());
            let optimum = loop {
                s.lin.cons[obj as usize].rhs = best - 1;
                s.restart();
                s.requeue(obj);
                s.propagate_root();
                match s.search(None, None) {
                    Search::Solution => best = objective_value(&m, &s.values()),
                    _ => break best,
                }
            };
            (best, x) = first;
            let mut lns = Lns::new(&m);
            for _ in 0..30 {
                if let Some(better) = lns.round(&m, &x, best, None, 200) {
                    assert!(feasible(&m, &better.x), "case {case}: infeasible improvement");
                    assert_eq!(better.value, objective_value(&m, &better.x));
                    assert!(better.value < best && better.value >= optimum, "case {case}");
                    (best, x) = (better.value, better.x);
                    improvements += 1;
                }
            }
        }
        assert!(improvements > 0, "LNS never improved");
    }
}
