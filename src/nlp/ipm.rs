//! A primal-dual interior point method with a filter line search, after
//! Wächter and Biegler, "On the implementation of an interior-point filter
//! line-search algorithm for large-scale nonlinear programming" (Math.
//! Programming 106, 2006), the algorithm of IPOPT.
//!
//! The problem is `min f(z)` subject to `c(z) = 0` and `l <= z <= u` (some
//! bounds infinite); the caller turns inequalities into equalities with
//! slacks. Each iteration solves the barrier problem's primal-dual Newton
//! system
//!
//! ```text
//! [ W + Sigma + dw I   J^T   ] [dz]     [ grad phi + J^T y ]
//! [ J                  -dc I ] [dy] = - [ c                ]
//! ```
//!
//! where W is the Hessian of the Lagrangian `f + y^T c`, Sigma comes from
//! the bound multipliers, and dw, dc correct the inertia (n positive and m
//! negative eigenvalues) when needed. Steps keep a fraction of the distance
//! to the bounds, and a filter of (infeasibility, barrier objective) pairs
//! accepts them; second-order corrections fight the Maratos effect. The
//! barrier parameter decreases monotonically (Fiacco–McCormick). When no
//! acceptable step exists, a feasibility restoration phase minimizes the
//! infeasibility (an l1 penalty with a proximity term) with the same
//! algorithm, until the original filter accepts the point; if that converges
//! without becoming feasible, the problem is locally infeasible.
//!
//! The Newton system is factored by the dense Bunch–Kaufman LDL^T (`ldl`)
//! for small or dense problems, and otherwise by the sparse LDL^T with a
//! fixed order (`sparse_ldl`), which needs a quasi-definite matrix: it
//! always factors with a small dc (DELTA_C mu^KAPPA_C, IPOPT's value for a
//! rank-deficient Jacobian), and iterative refinement then solves the
//! system with the dc the inertia correction chose (usually 0). When the
//! sparse factorization fails (a zero pivot that dw doesn't cure, or
//! refinement that doesn't converge), the iteration uses the dense one.

use super::ldl::{Inertia, Ldl};
use super::sparse_ldl::{self, Symbolic};
use std::time::Instant;

/// A problem in the standard form above. The Jacobian and the Hessian are
/// sparse, with fixed patterns: their values come in the patterns' order,
/// and entries at the same position are summed. The Jacobian's pattern
/// lists each row's entries together, rows in order.
pub trait Problem {
    fn n(&self) -> usize;
    fn m(&self) -> usize;
    fn lower(&self) -> &[f64];
    fn upper(&self) -> &[f64];
    /// The Jacobian's (row, column) pattern.
    fn jacobian_pattern(&self) -> Vec<(usize, usize)>;
    /// The Hessian's pattern, in the lower triangle (row >= column).
    fn hessian_pattern(&self) -> Vec<(usize, usize)>;
    /// f(z) and c(z), or None if they aren't finite there.
    fn eval(&mut self, z: &[f64]) -> Option<(f64, Vec<f64>)>;
    /// The gradient of f (dense) and the Jacobian's values at z.
    fn derivatives(&mut self, z: &[f64]) -> (Vec<f64>, Vec<f64>);
    /// The values of `sigma * Hess f + sum_i y_i Hess c_i` at z.
    fn hessian(&mut self, z: &[f64], sigma: f64, y: &[f64]) -> Vec<f64>;
    /// Called after each accepted step; true stops the solve (restoration).
    fn stop(&mut self, _z: &[f64]) -> bool {
        false
    }
    /// Whether the constraint values c at z are close enough to zero for a
    /// solution, in the problem's own (unscaled) units; the tolerance on the
    /// scaled problem must hold too.
    fn violation_ok(&self, _z: &[f64], _c: &[f64]) -> bool {
        true
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub tol: f64,
    pub acceptable_tol: f64,
    pub acceptable_iter: usize,
    pub max_iter: usize,
    pub mu_init: f64,
    pub deadline: Option<Instant>,
    pub debug: bool,
    /// Inside a restoration phase: no nested restoration.
    pub restoration: bool,
    /// Always the dense factorization (for comparisons).
    pub dense: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            tol: 1e-8,
            acceptable_tol: 1e-6,
            acceptable_iter: 15,
            max_iter: 3000,
            mu_init: 0.1,
            deadline: None,
            debug: false,
            restoration: false,
            dense: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The optimality error is within `tol`.
    Optimal,
    /// Within `acceptable_tol` for `acceptable_iter` iterations in a row.
    Acceptable,
    /// The restoration phase converged without reducing the infeasibility:
    /// a local minimizer of the infeasibility.
    LocallyInfeasible,
    MaxIterations,
    TimeLimit,
    /// The iterates grew without bound (perhaps the problem is unbounded).
    Diverging,
    /// `Problem::stop` asked to stop (the restoration phase succeeded).
    Stopped,
    /// No step could be computed (numerical trouble).
    Failed,
    /// The functions or their derivatives aren't finite at the start.
    BadStart,
}

pub struct Solution {
    pub status: Status,
    pub z: Vec<f64>,
    pub zl: Vec<f64>,
    pub zu: Vec<f64>,
    pub iterations: usize,
    /// The feasible iterate (`||c||_inf <= feasible_tol`) with the lowest f.
    pub best_feasible: Option<(f64, Vec<f64>)>,
}

/// Iterates count as feasible for `best_feasible` when `||c||_inf` is
/// at most this.
pub const FEASIBLE_TOL: f64 = 1e-7;

/// The sparse factorization is used for KKT systems of at least this size
/// whose factor L has at most SPARSE_MAX_FILL of the dense one's entries.
const SPARSE_MIN_SIZE: usize = 150;
const SPARSE_MAX_FILL: f64 = 0.3;
/// Iterative refinement: at most this many steps, until the componentwise
/// backward error is within REFINE_TOL.
const REFINE_STEPS: usize = 10;
const REFINE_TOL: f64 = 1e-12;
/// A sparse solve whose backward error stays above this falls back to the
/// dense factorization.
const REFINE_FAIL: f64 = 1e-8;

// Constants from the paper (and IPOPT's defaults).
const KAPPA_1: f64 = 1e-2; // bound push
const KAPPA_2: f64 = 1e-2; // bound fraction
const KAPPA_D: f64 = 1e-5; // damping of single-bounded variables
const KAPPA_EPS: f64 = 10.0;
const KAPPA_MU: f64 = 0.2;
const THETA_MU: f64 = 1.5;
const TAU_MIN: f64 = 0.99;
const KAPPA_SIGMA: f64 = 1e10;
const GAMMA_THETA: f64 = 1e-5;
const GAMMA_PHI: f64 = 1e-8;
const DELTA: f64 = 1.0;
const S_THETA: f64 = 1.1;
const S_PHI: f64 = 2.3;
const ETA_PHI: f64 = 1e-4;
const GAMMA_ALPHA: f64 = 0.05;
/// The line search gives up below this step size whatever its own minimum
/// is: such a step no longer moves the point. (At a feasible point the
/// minimum is zero, and rounding can keep every step from being accepted.)
const ALPHA_FLOOR: f64 = 1e-16;
const MAX_SOC: usize = 4;
const KAPPA_SOC: f64 = 0.99;
const S_MAX: f64 = 100.0;
// Inertia correction.
const DELTA_W_0: f64 = 1e-4;
const DELTA_W_MIN: f64 = 1e-20;
const DELTA_W_MAX: f64 = 1e40;
const KAPPA_W_MINUS: f64 = 1.0 / 3.0;
const KAPPA_W_PLUS: f64 = 8.0;
const KAPPA_W_PLUS_FIRST: f64 = 100.0;
const DELTA_C: f64 = 1e-8;
const KAPPA_C: f64 = 0.25;
// Restoration.
const RHO: f64 = 1000.0;
const KAPPA_RESTO: f64 = 0.9;
const DIVERGING: f64 = 1e20;
const BOUND_RELAX: f64 = 1e-8;

/// `a <= b`, allowing for rounding in quantities of the size of `base`.
fn le_rounded(a: f64, b: f64, base: f64) -> bool {
    a - b <= 10.0 * f64::EPSILON * base.abs()
}

fn norm_inf(v: &[f64]) -> f64 {
    v.iter().fold(0.0, |m, x| m.max(x.abs()))
}

fn norm_1(v: &[f64]) -> f64 {
    v.iter().map(|x| x.abs()).sum()
}

/// `J^T y` for a Jacobian with pattern `pattern` and values `j`.
fn jt_times(pattern: &[(usize, usize)], j: &[f64], n: usize, y: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0; n];
    for (&(r, c), &a) in pattern.iter().zip(j) {
        if y[r] != 0.0 {
            out[c] += a * y[r];
        }
    }
    out
}

/// The state of the iteration.
struct State {
    z: Vec<f64>,
    y: Vec<f64>,
    zl: Vec<f64>,
    zu: Vec<f64>,
    f: f64,
    c: Vec<f64>,
    grad: Vec<f64>,
    /// The Jacobian's values.
    jac: Vec<f64>,
}

/// The KKT system's sparse structure: the pattern (the Hessian's entries,
/// then the Jacobian's at rows n + r; the diagonal is implicit) and its
/// analysis.
struct SparseKkt {
    pattern: Vec<(usize, usize)>,
    sym: Symbolic,
}

/// A factored KKT matrix.
enum Kkt {
    Dense(Ldl),
    /// The factorization of the regularized matrix, and the values of the
    /// matrix the solves are for (the pattern's, then the diagonal).
    Sparse {
        factor: sparse_ldl::Factor,
        values: Vec<f64>,
        diag: Vec<f64>,
    },
}

impl Kkt {
    fn inertia(&self) -> Inertia {
        match self {
            Kkt::Dense(l) => l.inertia,
            Kkt::Sparse { factor, .. } => factor.inertia,
        }
    }
}

/// The interior point method on one problem.
struct Ipm<'p> {
    p: &'p mut dyn Problem,
    opts: Options,
    n: usize,
    m: usize,
    jac_pattern: Vec<(usize, usize)>,
    hess_pattern: Vec<(usize, usize)>,
    /// The sparse factorization's structure, if it is used.
    sparse: Option<SparseKkt>,
    l: Vec<f64>,
    u: Vec<f64>,
    has_l: Vec<bool>,
    has_u: Vec<bool>,
    mu: f64,
    tau: f64,
    filter: Vec<(f64, f64)>,
    theta_max: f64,
    theta_min: f64,
    /// The last nonzero inertia correction dw.
    last_dw: f64,
    iterations: usize,
    best_feasible: Option<(f64, Vec<f64>)>,
}

/// The result of computing a search direction.
struct Direction {
    dz: Vec<f64>,
    dy: Vec<f64>,
    dzl: Vec<f64>,
    dzu: Vec<f64>,
    /// The factorization, for second-order corrections.
    kkt: Kkt,
    rhs_x: Vec<f64>,
}

pub fn solve(p: &mut dyn Problem, z0: &[f64], opts: &Options) -> Solution {
    solve_from(p, z0, None, opts)
}

/// Solves from z0, with initial bound multipliers if given.
fn solve_from(p: &mut dyn Problem, z0: &[f64], mults: Option<(Vec<f64>, Vec<f64>)>, opts: &Options) -> Solution {
    let (n, m) = (p.n(), p.m());
    // Bounds relaxed slightly (IPOPT's bound_relax_factor), so that a
    // feasible set without an interior (such as x^2 + y^2 <= 1 with x >= 1)
    // still has one.
    let relax = |b: f64, sign: f64| if b.is_finite() { b + sign * BOUND_RELAX * b.abs().max(1.0) } else { b };
    let l: Vec<f64> = p.lower().iter().map(|&b| relax(b, -1.0)).collect();
    let u: Vec<f64> = p.upper().iter().map(|&b| relax(b, 1.0)).collect();
    let has_l: Vec<bool> = l.iter().map(|v| v.is_finite()).collect();
    let has_u: Vec<bool> = u.iter().map(|v| v.is_finite()).collect();
    let (jac_pattern, hess_pattern) = (p.jacobian_pattern(), p.hessian_pattern());
    let sparse = if opts.dense { None } else { sparse_kkt(n, m, &hess_pattern, &jac_pattern) };
    if opts.debug && !opts.restoration {
        match &sparse {
            Some(k) => crate::config::debug_log!("nlp: sparse KKT factorization, L with {} nonzeros", k.sym.nnz_l()),
            None => crate::config::debug_log!("nlp: dense KKT factorization"),
        }
    }
    let mut ipm = Ipm {
        p,
        opts: opts.clone(),
        n,
        m,
        jac_pattern,
        hess_pattern,
        sparse,
        l,
        u,
        has_l,
        has_u,
        mu: opts.mu_init,
        tau: TAU_MIN.max(1.0 - opts.mu_init),
        filter: Vec::new(),
        theta_max: 0.0,
        theta_min: 0.0,
        last_dw: 0.0,
        iterations: 0,
        best_feasible: None,
    };
    ipm.run(z0, mults)
}

/// The sparse structure of the KKT matrix, if the sparse factorization
/// pays: a system of at least SPARSE_MIN_SIZE whose L stays sparse.
fn sparse_kkt(n: usize, m: usize, hess: &[(usize, usize)], jac: &[(usize, usize)]) -> Option<SparseKkt> {
    let size = n + m;
    if size < SPARSE_MIN_SIZE {
        return None;
    }
    let mut pattern: Vec<(usize, usize)> = hess.iter().map(|&(i, j)| (i.max(j), i.min(j))).collect();
    pattern.extend(jac.iter().map(|&(r, c)| (n + r, c)));
    let sym = Symbolic::new(size, &pattern);
    let dense = size * (size - 1) / 2;
    (sym.nnz_l() as f64 <= SPARSE_MAX_FILL * dense as f64).then_some(SparseKkt { pattern, sym })
}

impl Ipm<'_> {
    fn fail(&self, status: Status, z: Vec<f64>) -> Solution {
        let n = z.len();
        Solution {
            status,
            z,
            zl: vec![0.0; n],
            zu: vec![0.0; n],
            iterations: self.iterations,
            best_feasible: self.best_feasible.clone(),
        }
    }

    /// Moves a starting point strictly inside the bounds (IPOPT's bound push).
    fn push_inside(&self, z0: &[f64]) -> Vec<f64> {
        (0..self.n)
            .map(|i| {
                let (l, u, x) = (self.l[i], self.u[i], z0[i]);
                match (self.has_l[i], self.has_u[i]) {
                    (true, true) => {
                        let pl = (KAPPA_1 * l.abs().max(1.0)).min(KAPPA_2 * (u - l));
                        let pu = (KAPPA_1 * u.abs().max(1.0)).min(KAPPA_2 * (u - l));
                        if l + pl < u - pu { x.clamp(l + pl, u - pu) } else { 0.5 * (l + u) }
                    }
                    (true, false) => x.max(l + KAPPA_1 * l.abs().max(1.0)),
                    (false, true) => x.min(u - KAPPA_1 * u.abs().max(1.0)),
                    (false, false) => x,
                }
            })
            .collect()
    }

    fn strictly_inside(&self, z0: &[f64]) -> Vec<f64> {
        (0..self.n)
            .map(|i| {
                let margin = |b: f64| 1e-12 * b.abs().max(1.0);
                let mut x = z0[i];
                if self.has_l[i] {
                    x = x.max(self.l[i] + margin(self.l[i]));
                }
                if self.has_u[i] {
                    x = x.min(self.u[i] - margin(self.u[i]));
                }
                x
            })
            .collect()
    }

    /// The barrier objective at a point with objective f.
    fn phi(&self, f: f64, z: &[f64]) -> f64 {
        let mut phi = f;
        for i in 0..self.n {
            match (self.has_l[i], self.has_u[i]) {
                (true, true) => phi -= self.mu * ((z[i] - self.l[i]).ln() + (self.u[i] - z[i]).ln()),
                (true, false) => phi += self.mu * (KAPPA_D * (z[i] - self.l[i]) - (z[i] - self.l[i]).ln()),
                (false, true) => phi += self.mu * (KAPPA_D * (self.u[i] - z[i]) - (self.u[i] - z[i]).ln()),
                (false, false) => {}
            }
        }
        phi
    }

    /// The barrier objective's gradient.
    fn grad_phi(&self, s: &State) -> Vec<f64> {
        (0..self.n)
            .map(|i| {
                let mut g = s.grad[i];
                let (dl, du) = (s.z[i] - self.l[i], self.u[i] - s.z[i]);
                if self.has_l[i] {
                    g -= self.mu / dl;
                    if !self.has_u[i] {
                        g += self.mu * KAPPA_D;
                    }
                }
                if self.has_u[i] {
                    g += self.mu / du;
                    if !self.has_l[i] {
                        g -= self.mu * KAPPA_D;
                    }
                }
                g
            })
            .collect()
    }

    /// The scaled optimality error of the barrier problem with parameter mu
    /// (0 for the original problem).
    fn error(&self, s: &State, mu: f64) -> f64 {
        let jty = jt_times(&self.jac_pattern, &s.jac, self.n, &s.y);
        let dual = (0..self.n).map(|i| (s.grad[i] + jty[i] - s.zl[i] + s.zu[i]).abs()).fold(0.0, f64::max);
        let mut compl: f64 = 0.0;
        for i in 0..self.n {
            if self.has_l[i] {
                compl = compl.max((s.zl[i] * (s.z[i] - self.l[i]) - mu).abs());
            }
            if self.has_u[i] {
                compl = compl.max((s.zu[i] * (self.u[i] - s.z[i]) - mu).abs());
            }
        }
        let bounds = self.has_l.iter().chain(&self.has_u).filter(|&&b| b).count();
        let zsum = norm_1(&s.zl) + norm_1(&s.zu);
        let s_d = (S_MAX.max((norm_1(&s.y) + zsum) / (self.m + bounds).max(1) as f64)) / S_MAX;
        let s_c = (S_MAX.max(zsum / bounds.max(1) as f64)) / S_MAX;
        (dual / s_d).max(norm_inf(&s.c)).max(compl / s_c)
    }

    /// Least-squares multipliers: min ||grad f + J^T y - zl + zu||.
    fn least_squares_y(&self, s: &State) -> Vec<f64> {
        let (n, m) = (self.n, self.m);
        if m == 0 {
            return Vec::new();
        }
        let mut rhs: Vec<f64> = (0..n).map(|i| -(s.grad[i] - s.zl[i] + s.zu[i])).collect();
        rhs.extend(std::iter::repeat_n(0.0, m));
        if let Some(sk) = &self.sparse {
            // [I J^T; J -DELTA_C I] is quasi-definite.
            let mut values = vec![0.0; self.hess_pattern.len()];
            values.extend_from_slice(&s.jac);
            let mut diag = vec![1.0; n];
            diag.extend(std::iter::repeat_n(-DELTA_C, m));
            let factor = sk.sym.factor(&values, &diag);
            let kkt = Kkt::Sparse { factor, values, diag };
            if let Some(x) = self.solve_sparse(&kkt, &rhs) {
                let y = x[n..].to_vec();
                return if norm_inf(&y) <= 1e3 { y } else { vec![0.0; m] };
            }
        }
        let size = n + m;
        let mut k = vec![0.0; size * size];
        for i in 0..n {
            k[i * size + i] = 1.0;
        }
        for (&(r, c), &v) in self.jac_pattern.iter().zip(&s.jac) {
            k[(n + r) * size + c] += v;
        }
        for r in 0..m {
            k[(n + r) * size + n + r] = -DELTA_C;
        }
        let ldl = Ldl::factor(size, &k);
        ldl.solve(&mut rhs);
        let y = rhs[n..].to_vec();
        if y.iter().all(|v| v.is_finite()) && norm_inf(&y) <= 1e3 { y } else { vec![0.0; m] }
    }

    fn evaluate(&mut self, z: Vec<f64>, y: Vec<f64>, zl: Vec<f64>, zu: Vec<f64>) -> Option<State> {
        let (f, c) = self.p.eval(&z)?;
        let (grad, jac) = self.p.derivatives(&z);
        if !grad.iter().chain(&jac).all(|v| v.is_finite()) {
            return None;
        }
        Some(State { z, y, zl, zu, f, c, grad, jac })
    }

    fn run(&mut self, z0: &[f64], mults: Option<(Vec<f64>, Vec<f64>)>) -> Solution {
        let (n, m) = (self.n, self.m);
        // A cold start is pushed well inside the bounds; a warm start (the
        // restoration phase's) only strictly inside them.
        let z = if mults.is_some() { self.strictly_inside(z0) } else { self.push_inside(z0) };
        let (zl, zu) = mults.unwrap_or_else(|| {
            (
                self.has_l.iter().map(|&b| if b { 1.0 } else { 0.0 }).collect(),
                self.has_u.iter().map(|&b| if b { 1.0 } else { 0.0 }).collect(),
            )
        });
        let Some(mut s) = self.evaluate(z.clone(), vec![0.0; m], zl, zu) else {
            return self.fail(Status::BadStart, z);
        };
        s.y = if self.opts.restoration { vec![0.0; m] } else { self.least_squares_y(&s) };
        let theta0 = norm_1(&s.c);
        self.theta_max = 1e4 * theta0.max(1.0);
        self.theta_min = 1e-4 * theta0.max(1.0);
        let mut acceptable_count = 0;
        let mut tiny_steps = 0;
        if self.opts.debug && !self.opts.restoration {
            crate::config::debug_log!("nlp: {n} variables, {m} equality constraints");
        }
        loop {
            // Record the best feasible iterate.
            if norm_inf(&s.c) <= FEASIBLE_TOL && self.best_feasible.as_ref().is_none_or(|b| s.f < b.0) {
                self.best_feasible = Some((s.f, s.z.clone()));
            }
            // Convergence.
            let e0 = self.error(&s, 0.0);
            let violation_ok = self.p.violation_ok(&s.z, &s.c);
            if e0 <= self.opts.tol && violation_ok {
                return self.finish(Status::Optimal, s);
            }
            if e0 <= self.opts.acceptable_tol && violation_ok {
                acceptable_count += 1;
                if acceptable_count >= self.opts.acceptable_iter {
                    return self.finish(Status::Acceptable, s);
                }
            } else {
                acceptable_count = 0;
            }
            if self.iterations >= self.opts.max_iter {
                return self.finish(Status::MaxIterations, s);
            }
            if crate::config::expired(self.opts.deadline) {
                return self.finish(Status::TimeLimit, s);
            }
            if norm_inf(&s.z) > DIVERGING {
                return self.finish(Status::Diverging, s);
            }
            // Barrier parameter update (possibly several times).
            let mu_min = self.opts.tol / 10.0;
            while self.mu > mu_min && (self.error(&s, self.mu) <= KAPPA_EPS * self.mu || tiny_steps >= 2) {
                self.mu = mu_min.max((KAPPA_MU * self.mu).min(self.mu.powf(THETA_MU)));
                self.tau = TAU_MIN.max(1.0 - self.mu);
                self.filter.clear();
                tiny_steps = 0;
            }
            self.iterations += 1;
            // Search direction.
            let Some(d) = self.direction(&s) else {
                if self.opts.debug {
                    crate::config::debug_log!("nlp: no search direction (inertia correction failed)");
                }
                match self.restore(&mut s) {
                    Ok(()) => continue,
                    Err(status) => return self.finish(status, s),
                }
            };
            let alpha_max = self.max_step(&s.z, &d.dz);
            let alpha_z = self.max_mult_step(&s, &d);
            let tiny = (0..n).all(|i| d.dz[i].abs() <= 10.0 * f64::EPSILON * (1.0 + s.z[i].abs()));
            let step = if tiny {
                tiny_steps += 1;
                Some((alpha_max, None, false))
            } else {
                tiny_steps = 0;
                self.line_search(&s, &d, alpha_max)
            };
            let Some((alpha, soc, f_type)) = step else {
                if self.opts.debug {
                    crate::config::debug_log!("nlp: the line search failed (alpha_max {alpha_max:.2e})");
                }
                match self.restore(&mut s) {
                    Ok(()) => continue,
                    Err(status) => return self.finish(status, s),
                }
            };
            // The step taken: the direction, or a second-order correction.
            let (dz, dy) = match &soc {
                Some((dz, dy)) => (dz.clone(), dy.clone()),
                None => (d.dz.clone(), d.dy.clone()),
            };
            let (theta, phi) = (norm_1(&s.c), self.phi(s.f, &s.z));
            if !f_type {
                self.filter.push(((1.0 - GAMMA_THETA) * theta, phi - GAMMA_PHI * theta));
            }
            let z: Vec<f64> = (0..n).map(|i| s.z[i] + alpha * dz[i]).collect();
            let y: Vec<f64> = (0..m).map(|r| s.y[r] + alpha * dy[r]).collect();
            let mut zl: Vec<f64> = (0..n).map(|i| s.zl[i] + alpha_z * d.dzl[i]).collect();
            let mut zu: Vec<f64> = (0..n).map(|i| s.zu[i] + alpha_z * d.dzu[i]).collect();
            // Keep the bound multipliers near mu / distance (kappa_sigma).
            for i in 0..n {
                if self.has_l[i] {
                    let g = self.mu / (z[i] - self.l[i]);
                    zl[i] = zl[i].min(KAPPA_SIGMA * g).max(g / KAPPA_SIGMA);
                }
                if self.has_u[i] {
                    let g = self.mu / (self.u[i] - z[i]);
                    zu[i] = zu[i].min(KAPPA_SIGMA * g).max(g / KAPPA_SIGMA);
                }
            }
            if self.opts.debug {
                crate::config::debug_log!(
                    "nlp{} {:4} f {:+.8e} inf_pr {:.2e} inf_du {:.2e} lg(mu) {:5.1} ||d|| {:.2e} alpha {:.2e}{}",
                    if self.opts.restoration { " resto" } else { "" },
                    self.iterations,
                    s.f,
                    norm_inf(&s.c),
                    self.error(&s, 0.0),
                    self.mu.log10(),
                    norm_inf(&dz),
                    alpha,
                    if soc.is_some() { " soc" } else { "" }
                );
            }
            match self.evaluate(z, y, zl, zu) {
                Some(next) => s = next,
                None => return self.finish(Status::Failed, s),
            }
            if self.p.stop(&s.z) {
                return self.finish(Status::Stopped, s);
            }
        }
    }

    fn finish(&self, status: Status, s: State) -> Solution {
        Solution {
            status,
            z: s.z,
            zl: s.zl,
            zu: s.zu,
            iterations: self.iterations,
            best_feasible: self.best_feasible.clone(),
        }
    }

    /// The Newton direction, with inertia correction; None if the
    /// correction fails.
    fn direction(&mut self, s: &State) -> Option<Direction> {
        let n = self.n;
        let hess = self.p.hessian(&s.z, 1.0, &s.y);
        let sigma: Vec<f64> = (0..n)
            .map(|i| {
                let mut sigma = 0.0;
                if self.has_l[i] {
                    sigma += s.zl[i] / (s.z[i] - self.l[i]);
                }
                if self.has_u[i] {
                    sigma += s.zu[i] / (self.u[i] - s.z[i]);
                }
                sigma
            })
            .collect();
        if !hess.iter().chain(&sigma).all(|v| v.is_finite()) {
            return None;
        }
        if self.sparse.is_some() {
            if let Some(d) = self.direction_by(s, &hess, &sigma, true) {
                return Some(d);
            }
            if self.opts.debug {
                crate::config::debug_log!("nlp: the sparse factorization failed; the dense one instead");
            }
        }
        self.direction_by(s, &hess, &sigma, false)
    }

    /// The direction with the sparse or the dense factorization.
    fn direction_by(&mut self, s: &State, hess: &[f64], sigma: &[f64], sparse: bool) -> Option<Direction> {
        let (n, m) = (self.n, self.m);
        let size = n + m;
        let mut base = Vec::new();
        if !sparse {
            base = vec![0.0; size * size];
            for (&(i, j), &v) in self.hess_pattern.iter().zip(hess) {
                base[i * size + j] += v;
            }
            for i in 0..n {
                base[i * size + i] += sigma[i];
            }
            for (&(r, c), &v) in self.jac_pattern.iter().zip(&s.jac) {
                base[(n + r) * size + c] += v;
            }
        }
        let mut values = Vec::new();
        if sparse {
            values = hess.to_vec();
            values.extend_from_slice(&s.jac);
        }
        // The sparse factorization's regularization (quasi-definiteness).
        let dc_min = DELTA_C * self.mu.powf(KAPPA_C);
        let sk = self.sparse.as_ref();
        let factor = |dw: f64, dc: f64| -> Kkt {
            if sparse {
                let mut diag: Vec<f64> = sigma.iter().map(|v| v + dw).collect();
                diag.extend(std::iter::repeat_n(-dc, m));
                let mut reg = diag.clone();
                for d in &mut reg[n..] {
                    *d = -dc.max(dc_min);
                }
                let factor = sk.expect("a sparse structure").sym.factor(&values, &reg);
                return Kkt::Sparse { factor, values: values.clone(), diag };
            }
            let mut k = base.clone();
            for i in 0..n {
                k[i * size + i] += dw;
            }
            for r in 0..m {
                k[(n + r) * size + n + r] -= dc;
            }
            Kkt::Dense(Ldl::factor(size, &k))
        };
        let good = |f: &Kkt| {
            let i = f.inertia();
            i.positive == n && i.negative == m && i.zero == 0
        };
        // Too few negative eigenvalues (or zero ones) mean a rank-deficient
        // Jacobian, which only dc fixes; too few positive ones need dw.
        let deficient = |f: &Kkt| {
            let i = f.inertia();
            i.zero > 0 || i.negative < m
        };
        let mut dc = 0.0;
        let mut kkt = factor(0.0, 0.0);
        if deficient(&kkt) {
            dc = DELTA_C * self.mu.powf(KAPPA_C);
            kkt = factor(0.0, dc);
        }
        let mut last_dw = self.last_dw;
        if !good(&kkt) {
            let mut dw = if last_dw == 0.0 { DELTA_W_0 } else { DELTA_W_MIN.max(KAPPA_W_MINUS * last_dw) };
            loop {
                kkt = factor(dw, dc);
                if deficient(&kkt) && dc == 0.0 {
                    // A rank-deficient Jacobian: regularize the constraints too.
                    dc = DELTA_C * self.mu.powf(KAPPA_C);
                    kkt = factor(dw, dc);
                }
                if good(&kkt) {
                    break;
                }
                dw *= if last_dw == 0.0 { KAPPA_W_PLUS_FIRST } else { KAPPA_W_PLUS };
                if dw > DELTA_W_MAX {
                    if self.opts.debug {
                        crate::config::debug_log!(
                            "nlp: inertia {:?} with dw {dw:.1e}, dc {dc:.1e} (n {n}, m {m})",
                            kkt.inertia()
                        );
                    }
                    return None;
                }
            }
            last_dw = dw;
        }
        let gphi = self.grad_phi(s);
        let jty = jt_times(&self.jac_pattern, &s.jac, n, &s.y);
        let rhs_x: Vec<f64> = (0..n).map(|i| -(gphi[i] + jty[i])).collect();
        let rhs_c: Vec<f64> = s.c.iter().map(|v| -v).collect();
        let (dz, dy) = self.solve_kkt(&kkt, &rhs_x, &rhs_c)?;
        self.last_dw = last_dw;
        let (dzl, dzu) = self.mult_steps(s, &dz);
        Some(Direction { dz, dy, dzl, dzu, kkt, rhs_x })
    }

    fn solve_kkt(&self, kkt: &Kkt, rhs_x: &[f64], rhs_c: &[f64]) -> Option<(Vec<f64>, Vec<f64>)> {
        let mut rhs = rhs_x.to_vec();
        rhs.extend_from_slice(rhs_c);
        match kkt {
            Kkt::Dense(ldl) => ldl.solve(&mut rhs),
            Kkt::Sparse { .. } => rhs = self.solve_sparse(kkt, &rhs)?,
        }
        if !rhs.iter().all(|v| v.is_finite()) {
            return None;
        }
        let dy = rhs.split_off(self.n);
        Some((rhs, dy))
    }

    /// A sparse solve with iterative refinement against the matrix's own
    /// values; None if the residual stays large.
    fn solve_sparse(&self, kkt: &Kkt, b: &[f64]) -> Option<Vec<f64>> {
        let (Some(sk), Kkt::Sparse { factor, values, diag }) = (&self.sparse, kkt) else { return None };
        if !factor.complete {
            return None;
        }
        let abs_values: Vec<f64> = values.iter().map(|v| v.abs()).collect();
        let abs_diag: Vec<f64> = diag.iter().map(|v| v.abs()).collect();
        let mut x = b.to_vec();
        factor.solve(&sk.sym, &mut x);
        // The best iterate so far, by its componentwise backward error
        // (Oettli–Prager): max |r_i| / (|b_i| + (|K| |x|)_i).
        let mut best: Option<(f64, Vec<f64>)> = None;
        for _ in 0..=REFINE_STEPS {
            let kx = sparse_ldl::multiply(&sk.pattern, values, diag, &x);
            let abs_x: Vec<f64> = x.iter().map(|v| v.abs()).collect();
            let size = sparse_ldl::multiply(&sk.pattern, &abs_values, &abs_diag, &abs_x);
            let r: Vec<f64> = b.iter().zip(&kx).map(|(b, k)| b - k).collect();
            let e = (0..r.len())
                .map(|i| {
                    let d = b[i].abs() + size[i];
                    if d > 0.0 {
                        r[i].abs() / d
                    } else if r[i] == 0.0 {
                        0.0
                    } else {
                        f64::INFINITY
                    }
                })
                .fold(0.0, f64::max);
            if !e.is_finite() || best.as_ref().is_some_and(|b| e >= b.0) {
                break;
            }
            best = Some((e, x.clone()));
            if e <= REFINE_TOL {
                break;
            }
            let mut d = r;
            factor.solve(&sk.sym, &mut d);
            for (x, d) in x.iter_mut().zip(&d) {
                *x += d;
            }
        }
        best.filter(|b| b.0 <= REFINE_FAIL).map(|b| b.1)
    }

    /// The bound multipliers' steps for a primal step dz.
    fn mult_steps(&self, s: &State, dz: &[f64]) -> (Vec<f64>, Vec<f64>) {
        let n = self.n;
        let mut dzl = vec![0.0; n];
        let mut dzu = vec![0.0; n];
        for i in 0..n {
            if self.has_l[i] {
                let dl = s.z[i] - self.l[i];
                dzl[i] = (self.mu - s.zl[i] * dl - s.zl[i] * dz[i]) / dl;
            }
            if self.has_u[i] {
                let du = self.u[i] - s.z[i];
                dzu[i] = (self.mu - s.zu[i] * du + s.zu[i] * dz[i]) / du;
            }
        }
        (dzl, dzu)
    }

    /// The largest step in (0, 1] that keeps a fraction tau of the distance
    /// to the bounds.
    fn max_step(&self, z: &[f64], dz: &[f64]) -> f64 {
        let mut alpha: f64 = 1.0;
        for i in 0..self.n {
            if self.has_l[i] && dz[i] < 0.0 {
                alpha = alpha.min(-self.tau * (z[i] - self.l[i]) / dz[i]);
            }
            if self.has_u[i] && dz[i] > 0.0 {
                alpha = alpha.min(self.tau * (self.u[i] - z[i]) / dz[i]);
            }
        }
        alpha
    }

    fn max_mult_step(&self, s: &State, d: &Direction) -> f64 {
        let mut alpha: f64 = 1.0;
        for i in 0..self.n {
            if self.has_l[i] && d.dzl[i] < 0.0 {
                alpha = alpha.min(-self.tau * s.zl[i] / d.dzl[i]);
            }
            if self.has_u[i] && d.dzu[i] < 0.0 {
                alpha = alpha.min(-self.tau * s.zu[i] / d.dzu[i]);
            }
        }
        alpha
    }

    fn acceptable_to_filter(&self, theta: f64, phi: f64) -> bool {
        self.filter.iter().all(|&(tf, pf)| theta < tf || !le_rounded(pf, phi, pf))
    }

    /// The backtracking filter line search. Returns the step size, the
    /// second-order corrected step if one was taken, and whether the step
    /// was an f-type (Armijo) step, which doesn't augment the filter; None
    /// if the step size fell below its minimum.
    #[allow(clippy::type_complexity)]
    fn line_search(
        &mut self,
        s: &State,
        d: &Direction,
        alpha_max: f64,
    ) -> Option<(f64, Option<(Vec<f64>, Vec<f64>)>, bool)> {
        let n = self.n;
        let theta = norm_1(&s.c);
        let phi = self.phi(s.f, &s.z);
        let gphi = self.grad_phi(s);
        let slope: f64 = (0..n).map(|i| gphi[i] * d.dz[i]).sum();
        let alpha_min = GAMMA_ALPHA
            * if slope < 0.0 {
                GAMMA_THETA.min(GAMMA_PHI * theta / -slope).min(DELTA * theta.powf(S_THETA) / (-slope).powf(S_PHI))
            } else {
                GAMMA_THETA
            };
        // Whether a trial point with (theta_t, phi_t) is accepted for step
        // size alpha of the original direction; and whether it is f-type.
        let accept = |ipm: &Self, alpha: f64, theta_t: f64, phi_t: f64| -> Option<bool> {
            if !(theta_t.is_finite() && phi_t.is_finite()) || theta_t > ipm.theta_max {
                return None;
            }
            if !ipm.acceptable_to_filter(theta_t, phi_t) {
                return None;
            }
            let switching = slope < 0.0 && alpha * (-slope).powf(S_PHI) > DELTA * theta.powf(S_THETA);
            if switching && theta <= ipm.theta_min {
                // Armijo on the barrier objective.
                return le_rounded(phi_t, phi + ETA_PHI * alpha * slope, phi).then_some(true);
            }
            (le_rounded(theta_t, (1.0 - GAMMA_THETA) * theta, theta) || le_rounded(phi_t, phi - GAMMA_PHI * theta, phi))
                .then_some(false)
        };
        let trial = |ipm: &mut Self, alpha: f64, dz: &[f64]| -> Option<(Vec<f64>, f64, Vec<f64>)> {
            let z: Vec<f64> = (0..n).map(|i| s.z[i] + alpha * dz[i]).collect();
            let (f, c) = ipm.p.eval(&z)?;
            Some((z, f, c))
        };
        let mut alpha = alpha_max;
        let mut first = true;
        loop {
            if alpha < alpha_min || alpha < ALPHA_FLOOR || alpha_min.is_nan() {
                return None;
            }
            match trial(self, alpha, &d.dz) {
                None => {
                    // Outside a function's domain: cut back.
                    alpha *= 0.5;
                    first = false;
                    continue;
                }
                Some((z_t, f_t, c_t)) => {
                    let (theta_t, phi_t) = (norm_1(&c_t), self.phi(f_t, &z_t));
                    if let Some(f_type) = accept(self, alpha, theta_t, phi_t) {
                        return Some((alpha, None, f_type));
                    }
                    if first && theta_t >= theta && self.m > 0 {
                        // Second-order corrections.
                        let mut c_soc: Vec<f64> = (0..self.m).map(|r| alpha * s.c[r] + c_t[r]).collect();
                        let mut theta_old = theta_t;
                        for _ in 0..MAX_SOC {
                            let rhs_c: Vec<f64> = c_soc.iter().map(|v| -v).collect();
                            let Some((dz_soc, dy_soc)) = self.solve_kkt(&d.kkt, &d.rhs_x, &rhs_c) else { break };
                            let alpha_soc = self.max_step(&s.z, &dz_soc);
                            let Some((z_s, f_s, c_s)) = trial(self, alpha_soc, &dz_soc) else { break };
                            let (theta_s, phi_s) = (norm_1(&c_s), self.phi(f_s, &z_s));
                            if let Some(f_type) = accept(self, alpha, theta_s, phi_s) {
                                // The corrected step alpha_soc * dz_soc replaces alpha * d.
                                return Some((alpha_soc, Some((dz_soc, dy_soc)), f_type));
                            }
                            if theta_s > KAPPA_SOC * theta_old {
                                break;
                            }
                            theta_old = theta_s;
                            c_soc = (0..self.m).map(|r| alpha_soc * c_soc[r] + c_s[r]).collect();
                        }
                    }
                }
            }
            first = false;
            alpha *= 0.5;
        }
    }

    /// The feasibility restoration phase: minimizes the infeasibility from
    /// the current point until the filter accepts a point. Updates s, or
    /// returns the final status (local infeasibility, limits).
    fn restore(&mut self, s: &mut State) -> Result<(), Status> {
        // No step is acceptable from a point that is nearly optimal already:
        // stop there instead (as IPOPT does).
        if self.error(s, 0.0) <= self.opts.acceptable_tol && self.p.violation_ok(&s.z, &s.c) {
            return Err(Status::Acceptable);
        }
        if self.opts.restoration {
            return Err(Status::Failed);
        }
        let (n, m) = (self.n, self.m);
        let theta = norm_1(&s.c);
        let phi = self.phi(s.f, &s.z);
        self.filter.push(((1.0 - GAMMA_THETA) * theta, phi - GAMMA_PHI * theta));
        if m == 0 {
            // Without constraints, no step means no progress at all.
            return Err(Status::Failed);
        }
        let mu_r = self.mu.max(norm_inf(&s.c));
        let mut resto = Restoration {
            orig: &mut *self.p,
            n,
            m,
            lower: self.l.iter().copied().chain(std::iter::repeat_n(0.0, 2 * m)).collect(),
            upper: self.u.iter().copied().chain(std::iter::repeat_n(f64::INFINITY, 2 * m)).collect(),
            z_ref: s.z.clone(),
            d: s.z.iter().map(|v| 1.0f64.min(1.0 / v.abs())).collect(),
            zeta: self.mu.sqrt(),
            filter: self.filter.clone(),
            mu: self.mu,
            l: self.l.clone(),
            u: self.u.clone(),
            theta_ref: theta,
        };
        // Starting p and n: the closed-form minimizers for this mu.
        let mut w0 = s.z.clone();
        let (mut p0, mut n0) = (vec![0.0; m], vec![0.0; m]);
        for r in 0..m {
            let a = (mu_r - RHO * s.c[r]) / (2.0 * RHO);
            n0[r] = a + (a * a + mu_r * s.c[r] / (2.0 * RHO)).sqrt();
            p0[r] = s.c[r] + n0[r];
        }
        w0.extend(&p0);
        w0.extend(&n0);
        let mut zl0: Vec<f64> = s.zl.iter().map(|v| v.min(RHO)).collect();
        let mut zu0: Vec<f64> = s.zu.iter().map(|v| v.min(RHO)).collect();
        zl0.extend(p0.iter().chain(&n0).map(|v| mu_r / v));
        zu0.extend(std::iter::repeat_n(0.0, 2 * m));
        let opts = Options {
            restoration: true,
            mu_init: mu_r,
            max_iter: self.opts.max_iter.saturating_sub(self.iterations),
            ..self.opts.clone()
        };
        if self.opts.debug {
            crate::config::debug_log!(
                "nlp: restoration phase at iteration {} (infeasibility {theta:.2e})",
                self.iterations
            );
        }
        let r = solve_from(&mut resto, &w0, Some((zl0, zu0)), &opts);
        self.iterations += r.iterations;
        let z: Vec<f64> = r.z[..n].to_vec();
        match r.status {
            Status::Stopped => {}
            Status::Optimal | Status::Acceptable => {
                // Converged without the filter accepting a point: locally
                // infeasible, unless the point is in fact feasible.
                let feasible = self.p.eval(&z).is_some_and(|(_, c)| norm_inf(&c) <= FEASIBLE_TOL);
                if !feasible {
                    return Err(Status::LocallyInfeasible);
                }
                self.filter.clear();
            }
            Status::TimeLimit => return Err(Status::TimeLimit),
            Status::MaxIterations => return Err(Status::MaxIterations),
            _ => return Err(Status::Failed),
        }
        // Continue from the restored point: its bound multipliers (reset if
        // large), and least-squares equality multipliers.
        let reset = |v: &[f64]| -> Vec<f64> { v.iter().map(|&x| if x > 1e3 { 1.0 } else { x }).collect() };
        let (zl, zu) = (reset(&r.zl[..n]), reset(&r.zu[..n]));
        let mut next = match self.evaluate(z, vec![0.0; m], zl, zu) {
            Some(next) => next,
            None => return Err(Status::Failed),
        };
        next.y = self.least_squares_y(&next);
        *s = next;
        self.last_dw = 0.0;
        Ok(())
    }
}

/// The restoration problem: in variables (z, p, n) >= (l, 0, 0),
/// `min rho * sum(p + n) + zeta / 2 * ||D (z - z_ref)||^2` subject to
/// `c(z) - p + n = 0`. It stops once the original filter accepts z with a
/// sufficiently smaller infeasibility.
struct Restoration<'a> {
    orig: &'a mut dyn Problem,
    n: usize,
    m: usize,
    lower: Vec<f64>,
    upper: Vec<f64>,
    z_ref: Vec<f64>,
    d: Vec<f64>,
    zeta: f64,
    /// The original problem's filter, barrier parameter, and bounds.
    filter: Vec<(f64, f64)>,
    mu: f64,
    l: Vec<f64>,
    u: Vec<f64>,
    theta_ref: f64,
}

impl Problem for Restoration<'_> {
    fn n(&self) -> usize {
        self.n + 2 * self.m
    }
    fn m(&self) -> usize {
        self.m
    }
    fn lower(&self) -> &[f64] {
        &self.lower
    }
    fn upper(&self) -> &[f64] {
        &self.upper
    }

    fn eval(&mut self, w: &[f64]) -> Option<(f64, Vec<f64>)> {
        let (n, m) = (self.n, self.m);
        let (_, c) = self.orig.eval(&w[..n])?;
        let (p, q) = (&w[n..n + m], &w[n + m..]);
        let prox: f64 = (0..n).map(|i| (self.d[i] * (w[i] - self.z_ref[i])).powi(2)).sum();
        let f = RHO * (p.iter().sum::<f64>() + q.iter().sum::<f64>()) + 0.5 * self.zeta * prox;
        let c: Vec<f64> = (0..m).map(|r| c[r] - p[r] + q[r]).collect();
        Some((f, c))
    }

    /// The original rows' entries, then each row's p and n, row by row.
    fn jacobian_pattern(&self) -> Vec<(usize, usize)> {
        let (n, m) = (self.n, self.m);
        let orig = self.orig.jacobian_pattern();
        let mut out = Vec::with_capacity(orig.len() + 2 * m);
        let mut k = 0;
        for r in 0..m {
            while k < orig.len() && orig[k].0 == r {
                out.push(orig[k]);
                k += 1;
            }
            out.push((r, n + r));
            out.push((r, n + m + r));
        }
        out
    }

    /// The original Hessian's pattern, then the proximity term's diagonal.
    fn hessian_pattern(&self) -> Vec<(usize, usize)> {
        let mut out = self.orig.hessian_pattern();
        out.extend((0..self.n).map(|i| (i, i)));
        out
    }

    fn derivatives(&mut self, w: &[f64]) -> (Vec<f64>, Vec<f64>) {
        let (n, m) = (self.n, self.m);
        let (_, jac) = self.orig.derivatives(&w[..n]);
        let orig = self.orig.jacobian_pattern();
        let mut grad: Vec<f64> = (0..n).map(|i| self.zeta * self.d[i] * self.d[i] * (w[i] - self.z_ref[i])).collect();
        grad.extend(std::iter::repeat_n(RHO, 2 * m));
        let mut jw = Vec::with_capacity(jac.len() + 2 * m);
        let mut k = 0;
        for r in 0..m {
            while k < orig.len() && orig[k].0 == r {
                jw.push(jac[k]);
                k += 1;
            }
            jw.push(-1.0);
            jw.push(1.0);
        }
        (grad, jw)
    }

    fn hessian(&mut self, w: &[f64], sigma: f64, y: &[f64]) -> Vec<f64> {
        let n = self.n;
        let mut h = self.orig.hessian(&w[..n], 0.0, y);
        h.extend((0..n).map(|i| sigma * self.zeta * self.d[i] * self.d[i]));
        h
    }

    fn stop(&mut self, w: &[f64]) -> bool {
        let z = &w[..self.n];
        let Some((f, c)) = self.orig.eval(z) else { return false };
        let theta = norm_1(&c);
        if theta > KAPPA_RESTO * self.theta_ref {
            return false;
        }
        // The original barrier objective, for the original filter.
        let mut phi = f;
        for i in 0..self.n {
            let (hl, hu) = (self.l[i].is_finite(), self.u[i].is_finite());
            if hl {
                phi -= self.mu * (z[i] - self.l[i]).ln();
                if !hu {
                    phi += self.mu * KAPPA_D * (z[i] - self.l[i]);
                }
            }
            if hu {
                phi -= self.mu * (self.u[i] - z[i]).ln();
                if !hl {
                    phi += self.mu * KAPPA_D * (self.u[i] - z[i]);
                }
            }
        }
        self.filter.iter().all(|&(tf, pf)| theta < tf || phi < pf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A problem from closures over dense derivatives (finite differences
    /// for the Hessian are not needed: the closures give it exactly): the
    /// Jacobian m x n row-major, the Hessian's lower triangle in n x n.
    struct Dense {
        n: usize,
        m: usize,
        l: Vec<f64>,
        u: Vec<f64>,
        #[allow(clippy::type_complexity)]
        f: Box<dyn Fn(&[f64]) -> Option<(f64, Vec<f64>)>>,
        #[allow(clippy::type_complexity)]
        d: Box<dyn Fn(&[f64]) -> (Vec<f64>, Vec<f64>)>,
        #[allow(clippy::type_complexity)]
        h: Box<dyn Fn(&[f64], f64, &[f64]) -> Vec<f64>>,
    }

    impl Problem for Dense {
        fn n(&self) -> usize {
            self.n
        }
        fn m(&self) -> usize {
            self.m
        }
        fn lower(&self) -> &[f64] {
            &self.l
        }
        fn upper(&self) -> &[f64] {
            &self.u
        }
        /// Every entry, row by row.
        fn jacobian_pattern(&self) -> Vec<(usize, usize)> {
            (0..self.m).flat_map(|r| (0..self.n).map(move |c| (r, c))).collect()
        }
        /// The whole lower triangle, row by row.
        fn hessian_pattern(&self) -> Vec<(usize, usize)> {
            (0..self.n).flat_map(|i| (0..=i).map(move |j| (i, j))).collect()
        }
        fn eval(&mut self, z: &[f64]) -> Option<(f64, Vec<f64>)> {
            (self.f)(z)
        }
        fn derivatives(&mut self, z: &[f64]) -> (Vec<f64>, Vec<f64>) {
            (self.d)(z)
        }
        fn hessian(&mut self, z: &[f64], sigma: f64, y: &[f64]) -> Vec<f64> {
            let h = (self.h)(z, sigma, y);
            let n = self.n;
            (0..n).flat_map(|i| (0..=i).map(move |j| (i, j))).map(|(i, j)| h[i * n + j]).collect()
        }
    }

    const INF: f64 = f64::INFINITY;

    #[test]
    fn rosenbrock() {
        let mut p = Dense {
            n: 2,
            m: 0,
            l: vec![-INF; 2],
            u: vec![INF; 2],
            f: Box::new(|x| Some((100.0 * (x[1] - x[0] * x[0]).powi(2) + (1.0 - x[0]).powi(2), vec![]))),
            d: Box::new(|x| {
                let g0 = -400.0 * x[0] * (x[1] - x[0] * x[0]) - 2.0 * (1.0 - x[0]);
                let g1 = 200.0 * (x[1] - x[0] * x[0]);
                (vec![g0, g1], vec![])
            }),
            h: Box::new(|x, s, _| {
                vec![s * (1200.0 * x[0] * x[0] - 400.0 * x[1] + 2.0), 0.0, s * -400.0 * x[0], s * 200.0]
            }),
        };
        let r = solve(&mut p, &[-1.2, 1.0], &Options::default());
        assert_eq!(r.status, Status::Optimal);
        assert!((r.z[0] - 1.0).abs() < 1e-6 && (r.z[1] - 1.0).abs() < 1e-6, "{:?}", r.z);
    }

    /// Hock–Schittkowski 71, IPOPT's example: optimum 17.0140173.
    fn hs071() -> Dense {
        Dense {
            n: 4,
            m: 2,
            l: vec![1.0; 4],
            u: vec![5.0; 4],
            // Both constraints as equalities (the inequality is active at
            // the optimum); slacks are the model layer's job.
            f: Box::new(|x| {
                let f = x[0] * x[3] * (x[0] + x[1] + x[2]) + x[2];
                Some((f, vec![x[0] * x[1] * x[2] * x[3] - 25.0, x.iter().map(|v| v * v).sum::<f64>() - 40.0]))
            }),
            d: Box::new(|x| {
                let g = vec![
                    x[3] * (2.0 * x[0] + x[1] + x[2]),
                    x[0] * x[3],
                    x[0] * x[3] + 1.0,
                    x[0] * (x[0] + x[1] + x[2]),
                ];
                let j = vec![
                    x[1] * x[2] * x[3],
                    x[0] * x[2] * x[3],
                    x[0] * x[1] * x[3],
                    x[0] * x[1] * x[2],
                    2.0 * x[0],
                    2.0 * x[1],
                    2.0 * x[2],
                    2.0 * x[3],
                ];
                (g, j)
            }),
            h: Box::new(|x, s, y| {
                let mut h = vec![0.0; 16];
                let mut set = |i: usize, j: usize, v: f64| h[i * 4 + j] += v;
                // objective
                set(0, 0, s * 2.0 * x[3]);
                set(1, 0, s * x[3]);
                set(2, 0, s * x[3]);
                set(3, 0, s * (2.0 * x[0] + x[1] + x[2]));
                set(3, 1, s * x[0]);
                set(3, 2, s * x[0]);
                // product
                set(1, 0, y[0] * x[2] * x[3]);
                set(2, 0, y[0] * x[1] * x[3]);
                set(3, 0, y[0] * x[1] * x[2]);
                set(2, 1, y[0] * x[0] * x[3]);
                set(3, 1, y[0] * x[0] * x[2]);
                set(3, 2, y[0] * x[0] * x[1]);
                // sum of squares
                for i in 0..4 {
                    set(i, i, 2.0 * y[1]);
                }
                h
            }),
        }
    }

    #[test]
    fn hs071_with_equalities() {
        // With x1 x2 x3 x4 = 25 (active at the optimum), the optimum is the same.
        let mut p = hs071();
        let r = solve(&mut p, &[1.0, 5.0, 5.0, 1.0], &Options::default());
        assert_eq!(r.status, Status::Optimal);
        let f = (p.f)(&r.z).unwrap().0;
        assert!((f - 17.0140173).abs() < 1e-6, "f = {f}, z = {:?}", r.z);
    }

    /// A chain: min sum (x_i - 1)^2 subject to x_i^2 + x_{i+1}^2 = 1, with
    /// sparse derivatives, large enough for the sparse factorization. The
    /// optimum has every x_i = 1/sqrt(2); the dense factorization takes the
    /// same steps.
    struct Chain {
        n: usize,
        l: Vec<f64>,
        u: Vec<f64>,
    }

    impl Problem for Chain {
        fn n(&self) -> usize {
            self.n
        }
        fn m(&self) -> usize {
            self.n - 1
        }
        fn lower(&self) -> &[f64] {
            &self.l
        }
        fn upper(&self) -> &[f64] {
            &self.u
        }
        fn jacobian_pattern(&self) -> Vec<(usize, usize)> {
            (0..self.n - 1).flat_map(|r| [(r, r), (r, r + 1)]).collect()
        }
        fn hessian_pattern(&self) -> Vec<(usize, usize)> {
            (0..self.n).map(|i| (i, i)).collect()
        }
        fn eval(&mut self, x: &[f64]) -> Option<(f64, Vec<f64>)> {
            let f = x.iter().map(|v| (v - 1.0).powi(2)).sum();
            Some((f, (0..self.n - 1).map(|r| x[r] * x[r] + x[r + 1] * x[r + 1] - 1.0).collect()))
        }
        fn derivatives(&mut self, x: &[f64]) -> (Vec<f64>, Vec<f64>) {
            let g = x.iter().map(|v| 2.0 * (v - 1.0)).collect();
            (g, (0..self.n - 1).flat_map(|r| [2.0 * x[r], 2.0 * x[r + 1]]).collect())
        }
        fn hessian(&mut self, _x: &[f64], sigma: f64, y: &[f64]) -> Vec<f64> {
            (0..self.n)
                .map(|i| {
                    let mut h = 2.0 * sigma;
                    if i > 0 {
                        h += 2.0 * y[i - 1];
                    }
                    if i + 1 < self.n {
                        h += 2.0 * y[i];
                    }
                    h
                })
                .collect()
        }
    }

    #[test]
    fn sparse_factorization() {
        let n = 100;
        let mut p = Chain { n, l: vec![-10.0; n], u: vec![10.0; n] };
        assert!(sparse_kkt(n, n - 1, &p.hessian_pattern(), &p.jacobian_pattern()).is_some());
        let z0: Vec<f64> = (0..n).map(|i| 0.3 + 0.01 * i as f64).collect();
        let sparse = solve(&mut p, &z0, &Options::default());
        assert_eq!(sparse.status, Status::Optimal);
        for v in &sparse.z {
            assert!((v - 0.5f64.sqrt()).abs() < 1e-7, "{:?}", sparse.z);
        }
        let dense = solve(&mut p, &z0, &Options { dense: true, ..Options::default() });
        assert_eq!(dense.status, Status::Optimal);
        assert_eq!(dense.iterations, sparse.iterations);
        for (a, b) in sparse.z.iter().zip(&dense.z) {
            assert!((a - b).abs() < 1e-10);
        }
    }

    #[test]
    fn locally_infeasible() {
        // x^2 + 1 = 0 has no solution; the infeasibility x^2 + 1 is minimal at x = 0.
        let mut p = Dense {
            n: 1,
            m: 1,
            l: vec![-INF],
            u: vec![INF],
            f: Box::new(|x| Some((x[0], vec![x[0] * x[0] + 1.0]))),
            d: Box::new(|x| (vec![1.0], vec![2.0 * x[0]])),
            h: Box::new(|_, _, y| vec![2.0 * y[0]]),
        };
        let r = solve(&mut p, &[3.0], &Options::default());
        assert_eq!(r.status, Status::LocallyInfeasible);
    }

    #[test]
    fn bounds_and_a_domain() {
        // min -ln(x) + x/2 on x >= 0 (ln undefined at x <= 0): x = 2.
        let mut p = Dense {
            n: 1,
            m: 0,
            l: vec![0.0],
            u: vec![INF],
            f: Box::new(|x| {
                let f = -x[0].ln() + 0.5 * x[0];
                f.is_finite().then(|| (f, vec![]))
            }),
            d: Box::new(|x| (vec![-1.0 / x[0] + 0.5], vec![])),
            h: Box::new(|x, s, _| vec![s / (x[0] * x[0])]),
        };
        let r = solve(&mut p, &[10.0], &Options::default());
        assert_eq!(r.status, Status::Optimal);
        assert!((r.z[0] - 2.0).abs() < 1e-7, "{:?}", r.z);
        // An active bound: min x on 1 <= x <= 3.
        let mut p = Dense {
            n: 1,
            m: 0,
            l: vec![1.0],
            u: vec![3.0],
            f: Box::new(|x| Some((x[0], vec![]))),
            d: Box::new(|_| (vec![1.0], vec![])),
            h: Box::new(|_, _, _| vec![0.0]),
        };
        let r = solve(&mut p, &[2.0], &Options::default());
        assert_eq!(r.status, Status::Optimal);
        assert!((r.z[0] - 1.0).abs() < 1e-7, "{:?}", r.z);
    }
}
