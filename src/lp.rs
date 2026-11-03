//! A linear program in the form HiGHS uses (`HighsLp`):
//!
//!   minimize  c^T x  subject to  row_lower <= A x <= row_upper,
//!                                col_lower <=  x  <= col_upper
//!
//! with A stored both column-wise and row-wise.

use crate::model::{LinearModel, Sense};

#[derive(Clone)]
pub struct Lp {
    pub num_col: usize,
    pub num_row: usize,
    pub col_cost: Vec<f64>,
    pub col_lower: Vec<f64>,
    pub col_upper: Vec<f64>,
    pub row_lower: Vec<f64>,
    pub row_upper: Vec<f64>,
    pub a_start: Vec<usize>,
    pub a_index: Vec<usize>,
    pub a_value: Vec<f64>,
    pub ar_start: Vec<usize>,
    pub ar_index: Vec<usize>,
    pub ar_value: Vec<f64>,
}

impl Lp {
    /// Builds the minimization LP for a model. Maximization is negated, as
    /// HiGHS does with `sense_`; `zero_cost` drops the objective entirely.
    pub fn from_model(model: &LinearModel, zero_cost: bool) -> Lp {
        let n = model.num_cols();
        let m = model.rows.len();
        let sign = if model.sense == Sense::Maximize { -1.0 } else { 1.0 };
        let col_cost = if zero_cost { vec![0.0; n] } else { model.obj.iter().map(|c| sign * c).collect() };

        let mut ar_start = Vec::with_capacity(m + 1);
        let (mut ar_index, mut ar_value) = (Vec::new(), Vec::new());
        ar_start.push(0);
        for row in &model.rows {
            for &(j, v) in &row.coefs {
                ar_index.push(j);
                ar_value.push(v);
            }
            ar_start.push(ar_index.len());
        }
        // Column-wise copy by counting sort.
        let mut a_start = vec![0usize; n + 1];
        for &j in &ar_index {
            a_start[j + 1] += 1;
        }
        for j in 0..n {
            a_start[j + 1] += a_start[j];
        }
        let mut next = a_start.clone();
        let mut a_index = vec![0usize; ar_index.len()];
        let mut a_value = vec![0.0; ar_index.len()];
        for i in 0..m {
            for k in ar_start[i]..ar_start[i + 1] {
                let j = ar_index[k];
                a_index[next[j]] = i;
                a_value[next[j]] = ar_value[k];
                next[j] += 1;
            }
        }
        Lp {
            num_col: n,
            num_row: m,
            col_cost,
            col_lower: model.col_lower.clone(),
            col_upper: model.col_upper.clone(),
            row_lower: model.rows.iter().map(|r| r.lower).collect(),
            row_upper: model.rows.iter().map(|r| r.upper).collect(),
            a_start,
            a_index,
            a_value,
            ar_start,
            ar_index,
            ar_value,
        }
    }

    pub fn objective(&self, x: &[f64]) -> f64 {
        self.col_cost.iter().zip(x).map(|(c, v)| c * v).sum()
    }

    /// Rounds the bounds of each row whose columns are all integer, with
    /// integer coefficients, inward to multiples of the coefficients' gcd,
    /// since the row's value must be such a multiple. Returns false if a row's
    /// range becomes empty, which proves the problem infeasible (for example
    /// `2x - 2y = 1`), something branch and bound alone can't always show.
    pub fn tighten_integer_rows(&mut self, col_integer: &[bool]) -> bool {
        const EPS: f64 = 1e-9;
        for i in 0..self.num_row {
            let mut g: i64 = 0;
            for k in self.ar_start[i]..self.ar_start[i + 1] {
                let v = self.ar_value[k];
                if !col_integer[self.ar_index[k]] || v.fract() != 0.0 || v.abs() > 1e15 {
                    g = 0;
                    break;
                }
                g = gcd(g, v.abs() as i64);
            }
            if g == 0 {
                continue;
            }
            let g = g as f64;
            let (lo, hi) = (&mut self.row_lower[i], &mut self.row_upper[i]);
            if lo.is_finite() {
                *lo = (*lo / g - EPS).ceil() * g;
            }
            if hi.is_finite() {
                *hi = (*hi / g + EPS).floor() * g;
            }
            if *lo > *hi {
                return false;
            }
        }
        true
    }
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a } else { gcd(b, a % b) }
}
