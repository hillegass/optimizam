//! Generated models shared by the cancellation and progress tests.

#![allow(dead_code)] // each test file uses some of these

use optimizam::{Expression, IntervalVar, Model, Solution, Variable, constraint, var};

/// A small deterministic generator (the tests mustn't depend on a crate).
pub struct Lcg(pub u64);

impl Lcg {
    pub fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// A multidimensional knapsack with weak bounds (a large search tree), as in
/// `tools/milp_benchmark.py`: n binaries, m capacity rows, maximize value.
pub struct Knapsack {
    pub model: Model,
    pub x: Vec<Variable>,
    w: Vec<Vec<i64>>,
    v: Vec<i64>,
    cap: Vec<i64>,
}

impl Knapsack {
    pub fn new(n: usize, m: usize) -> Knapsack {
        let mut rng = Lcg(2);
        let w: Vec<Vec<i64>> = (0..m).map(|_| (0..n).map(|_| 5 + rng.below(56) as i64).collect()).collect();
        let v: Vec<i64> =
            (0..n).map(|j| (0..m).map(|r| w[r][j]).sum::<i64>() / m as i64 + rng.below(31) as i64).collect();
        let cap: Vec<i64> = w.iter().map(|row| row.iter().sum::<i64>() / 2).collect();
        let mut model = Model::new();
        let x = model.add_vars(n, var().binary().name("x"));
        for r in 0..m {
            let used: Expression = x.iter().zip(&w[r]).map(|(&x, &a)| a as f64 * x).sum();
            model.add(constraint!(used <= cap[r] as f64));
        }
        model.maximize(x.iter().zip(&v).map(|(&x, &c)| c as f64 * x).sum::<Expression>());
        Knapsack { model, x, w, v, cap }
    }

    /// Checks that `s` fits every capacity, and returns its value.
    pub fn check(&self, s: &Solution) -> f64 {
        for (r, row) in self.w.iter().enumerate() {
            let used: f64 = self.x.iter().zip(row).map(|(&x, &a)| a as f64 * s[x]).sum();
            assert!(used <= self.cap[r] as f64 + 1e-6, "row {r}: {used} > {}", self.cap[r]);
        }
        let value: f64 = self.x.iter().zip(&self.v).map(|(&x, &c)| c as f64 * s[x]).sum();
        assert!((value - s.objective().unwrap()).abs() < 1e-6, "objective {:?} vs {value}", s.objective());
        value
    }
}

/// A random job shop: each job visits every machine once, in a random
/// order; minimize the makespan. Solved by the CP engine.
pub struct JobShop {
    pub model: Model,
    pub makespan: Variable,
    /// (interval, start, duration) per machine.
    tasks: Vec<Vec<(IntervalVar, Variable, i64)>>,
}

impl JobShop {
    pub fn new(jobs: usize, machines: usize, seed: u64) -> JobShop {
        let mut rng = Lcg(seed);
        let mut m = Model::new();
        let horizon = (jobs * machines * 99) as f64;
        let makespan = m.add_var(var().integer().bounds(0, horizon).name("makespan"));
        let mut tasks = vec![vec![]; machines];
        for j in 0..jobs {
            let mut order: Vec<usize> = (0..machines).collect();
            for k in (1..machines).rev() {
                order.swap(k, rng.below(k as u64 + 1) as usize);
            }
            let mut previous: Option<(Variable, i64)> = None;
            for &machine in &order {
                let duration = 1 + rng.below(99) as i64;
                let start = m.add_var(var().integer().bounds(0, horizon).name(format!("s{j}_{machine}")));
                if let Some((p, d)) = previous {
                    m.add(constraint!(p + d as f64 <= start));
                }
                tasks[machine].push((m.interval(start, duration as f64), start, duration));
                previous = Some((start, duration));
            }
            let (last, d) = previous.unwrap();
            m.add(constraint!(last + d as f64 <= makespan));
        }
        for on in &tasks {
            m.no_overlap(&on.iter().map(|t| t.0).collect::<Vec<_>>());
        }
        m.minimize(makespan);
        JobShop { model: m, makespan, tasks }
    }

    /// Checks that no machine runs two tasks at once and that the makespan
    /// covers every task; returns the makespan.
    pub fn check(&self, s: &Solution) -> f64 {
        for on in &self.tasks {
            for (a, &(_, sa, da)) in on.iter().enumerate() {
                for &(_, sb, db) in &on[a + 1..] {
                    let (a0, b0) = (s[sa], s[sb]);
                    assert!(a0 + da as f64 <= b0 || b0 + db as f64 <= a0, "tasks overlap");
                }
            }
        }
        let end = self.tasks.iter().flatten().map(|&(_, st, d)| s[st] + d as f64).fold(0.0, f64::max);
        assert!(end <= s[self.makespan]);
        s[self.makespan]
    }
}
