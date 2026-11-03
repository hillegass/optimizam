//! Gomory mixed-integer (GMI) cuts from rows of the optimal simplex tableau.
//!
//! A tableau row reads x_v + sum(alpha_k x_k) = 0 over the nonbasic variables
//! k (structurals and row logicals s_i = -A_i x). Each nonbasic variable sits
//! at a bound, so substitute y_k = x_k - l_k (at lower) or y_k = u_k - x_k
//! (at upper), y_k >= 0, giving x_v + sum(a_k y_k) = beta, with beta the LP
//! value of x_v and y = 0 at the LP point. With f0 = frac(beta), the GMI cut
//! is sum(g_k y_k) >= 1, where for integer y_k (f_k = frac(a_k))
//! g_k = f_k / f0 if f_k <= f0, else (1 - f_k) / (1 - f0), and for continuous
//! y_k g_k = a_k / f0 if a_k >= 0, else -a_k / (1 - f0). The LP point has
//! sum(g_k y_k) = 0, so it is cut off. The cut is then mapped back to the
//! structural columns.
//!
//! A row logical counts as integer when its row has only integer columns with
//! integer coefficients and integer (or infinite) bounds.

use super::{Cut, Round, Source, finish};
use crate::lp::Lp;
use crate::simplex::{self, Basis};

/// Basic integer columns closer than this to an integer give weak, unstable cuts.
const MIN_FRACTIONALITY: f64 = 0.005;
/// Most tableau rows examined per round.
const MAX_SOURCES: usize = 200;
/// Tolerance for deciding that a nonbasic variable is at a bound.
const AT_BOUND: f64 = 1e-7;

/// Whether each row logical is integer-valued at every integer-feasible point.
fn integral_rows(lp: &Lp, integer: &[bool]) -> Vec<bool> {
    let is_int = |v: f64| !v.is_finite() || v.fract() == 0.0;
    (0..lp.num_row)
        .map(|i| {
            is_int(lp.row_lower[i])
                && is_int(lp.row_upper[i])
                && (lp.ar_start[i]..lp.ar_start[i + 1])
                    .all(|k| integer[lp.ar_index[k]] && lp.ar_value[k].fract() == 0.0)
        })
        .collect()
}

pub(super) fn separate(round: &Round, basis: &Basis) -> Vec<Cut> {
    let (lp, x) = (round.lp, round.x);
    let (n, m) = (lp.num_col, lp.num_row);

    // Values and bounds of all n + m variables; logical i is s_i = -A_i x.
    let mut value = x.to_vec();
    let (mut lo, mut hi) = (round.lower.to_vec(), round.upper.to_vec());
    let mut int_var = round.integer.to_vec();
    let int_rows = integral_rows(lp, round.integer);
    for i in 0..m {
        let ax: f64 = (lp.ar_start[i]..lp.ar_start[i + 1]).map(|k| lp.ar_value[k] * x[lp.ar_index[k]]).sum();
        value.push(-ax);
        lo.push(-lp.row_upper[i]);
        hi.push(-lp.row_lower[i]);
        int_var.push(int_rows[i]);
    }

    // Fractional basic integer columns, most fractional first.
    let mut sources: Vec<(usize, f64)> = basis
        .basic_index
        .iter()
        .filter(|&&v| v < n && round.integer[v])
        .map(|&v| (v, x[v] - x[v].floor()))
        .filter(|&(_, f)| (MIN_FRACTIONALITY..=1.0 - MIN_FRACTIONALITY).contains(&f))
        .collect();
    sources.sort_by(|a, b| (a.1 - 0.5).abs().total_cmp(&(b.1 - 0.5).abs()));
    sources.truncate(MAX_SOURCES);
    let vars: Vec<usize> = sources.iter().map(|&(v, _)| v).collect();
    let Some(rows) = simplex::tableau_rows(lp, basis, &vars) else { return Vec::new() };

    sources
        .iter()
        .zip(&rows)
        .filter_map(|(&(_, f0), row)| {
            let (coefs, rhs) = gmi_terms(lp, &value, &lo, &hi, &int_var, f0, row)?;
            finish(round, coefs, rhs, Source::Gmi)
        })
        .collect()
}

/// The raw GMI cut `coefs . x >= rhs` over the structural columns, or None
/// if a nonbasic variable lies strictly between its bounds.
fn gmi_terms(
    lp: &Lp,
    value: &[f64],
    lo: &[f64],
    hi: &[f64],
    int_var: &[bool],
    f0: f64,
    row: &[(usize, f64)],
) -> Option<(Vec<(usize, f64)>, f64)> {
    let n = lp.num_col;
    let mut coefs = Vec::new();
    let mut rhs = 1.0;
    for &(k, alpha) in row {
        let (l, u, v) = (lo[k], hi[k], value[k]);
        if l == u {
            continue; // fixed: y_k = 0 at every feasible point
        }
        let at_lower = l.is_finite() && (v - l).abs() <= AT_BOUND * (1.0 + l.abs());
        let at_upper = u.is_finite() && (v - u).abs() <= AT_BOUND * (1.0 + u.abs());
        if !at_lower && !at_upper {
            return None;
        }
        let a = if at_lower { alpha } else { -alpha };
        let g = if int_var[k] {
            let f = a - a.floor();
            if f <= f0 { f / f0 } else { (1.0 - f) / (1.0 - f0) }
        } else if a >= 0.0 {
            a / f0
        } else {
            -a / (1.0 - f0)
        };
        if g == 0.0 {
            continue;
        }
        // g * y_k with y_k = x_k - l (at lower) or u - x_k (at upper).
        let (c, shift) = if at_lower { (g, g * l) } else { (-g, -g * u) };
        rhs += shift;
        if k < n {
            coefs.push((k, c));
        } else {
            // x_k = s_i = -A_i x
            let i = k - n;
            for q in lp.ar_start[i]..lp.ar_start[i + 1] {
                coefs.push((lp.ar_index[q], -c * lp.ar_value[q]));
            }
        }
    }
    Some((coefs, rhs))
}
