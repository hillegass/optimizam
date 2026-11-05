//! Sparse symmetric factorization `P A P^T = L D L^T` with 1x1 pivots in a
//! fixed order (after QDLDL, the factorization of OSQP and Clarabel: Stellato
//! et al.; Goulart and Chen), which also gives the inertia of A from the
//! signs of D (Sylvester's law of inertia).
//!
//! Without pivoting, the factorization exists for any order when A is
//! quasi-definite, `[H J^T; J -C]` with H and C positive definite: the KKT
//! matrices of the interior point method are, once the inertia correction
//! and a small constraint regularization are in. For other matrices it can
//! meet a zero pivot (reported as a zero eigenvalue) or lose accuracy, which
//! the caller detects from the residual of its solves.
//!
//! The work splits in two. [`Symbolic::new`] takes the pattern (fixed for a
//! whole local solve): a minimum degree ordering, which keeps L sparse, the
//! elimination tree, and the number of nonzeros in each column of L.
//! [`Symbolic::factor`] then takes the values, once per factorization.

use super::ldl::Inertia;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const NONE: usize = usize::MAX;

/// Pivots this small relative to the largest entry count as zero (as in the
/// dense factorization: KKT matrices mix entries of 1e10 with
/// regularizations of 1e-11).
const ZERO_PIVOT: f64 = 1e-30;

/// The pattern's analysis.
pub struct Symbolic {
    n: usize,
    /// `perm[k]`: the row and column of A eliminated k-th.
    perm: Vec<usize>,
    /// The upper triangle of `P A P^T` by columns: column k's rows in
    /// `rows[col_start[k]..col_start[k + 1]]`, sorted, the diagonal last.
    col_start: Vec<usize>,
    rows: Vec<usize>,
    /// For each entry of the input pattern, its slot in those columns
    /// (entries at the same position share one), and each diagonal's slot.
    slot: Vec<usize>,
    diag_slot: Vec<usize>,
    /// The elimination tree, and the columns of L: column k's entries at
    /// `l_start[k]..l_start[k + 1]`.
    parent: Vec<usize>,
    l_start: Vec<usize>,
}

/// The numeric factorization.
pub struct Factor {
    l_rows: Vec<usize>,
    l_values: Vec<f64>,
    d_inv: Vec<f64>,
    pub inertia: Inertia,
    /// False after a zero pivot: the factorization stopped there, and only
    /// the inertia counts so far are meaningful.
    pub complete: bool,
}

impl Symbolic {
    /// The analysis of an n x n symmetric matrix's pattern, given as
    /// lower-triangle entries `(i, j)` with i >= j (repeats allowed). Every
    /// diagonal entry is part of the pattern, given or not.
    pub fn new(n: usize, pattern: &[(usize, usize)]) -> Symbolic {
        // The adjacency graph, for the ordering.
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &(i, j) in pattern {
            debug_assert!(i >= j && i < n, "a lower-triangle entry inside the matrix");
            if i != j {
                adj[i].push(j);
                adj[j].push(i);
            }
        }
        for a in &mut adj {
            a.sort_unstable();
            a.dedup();
        }
        let perm = minimum_degree(adj);
        let mut iperm = vec![0; n];
        for (k, &v) in perm.iter().enumerate() {
            iperm[v] = k;
        }
        // The permuted upper triangle: (column, row, input entry), with the
        // diagonal entries (entry index NONE when not given).
        let mut entries: Vec<(usize, usize, usize)> = pattern
            .iter()
            .enumerate()
            .map(|(e, &(i, j))| {
                let (a, b) = (iperm[i], iperm[j]);
                (a.max(b), a.min(b), e)
            })
            .collect();
        entries.extend((0..n).map(|k| (k, k, NONE)));
        entries.sort_unstable();
        let mut col_start = vec![0; n + 1];
        let mut rows = Vec::with_capacity(entries.len());
        let mut slot = vec![0; pattern.len()];
        let mut diag_slot = vec![0; n];
        let mut last = (NONE, NONE);
        for &(c, r, e) in &entries {
            if (c, r) != last {
                rows.push(r);
                col_start[c + 1] = rows.len();
                last = (c, r);
            }
            let s = rows.len() - 1;
            if e != NONE {
                slot[e] = s;
            }
            if r == c {
                diag_slot[perm[c]] = s;
            }
        }
        for k in 0..n {
            col_start[k + 1] = col_start[k + 1].max(col_start[k]);
        }
        // The elimination tree and the column counts of L (QDLDL_etree).
        let mut parent = vec![NONE; n];
        let mut count = vec![0; n];
        let mut mark = vec![NONE; n];
        for j in 0..n {
            mark[j] = j;
            for &r in &rows[col_start[j]..col_start[j + 1]] {
                let mut i = r;
                while mark[i] != j {
                    if parent[i] == NONE {
                        parent[i] = j;
                    }
                    count[i] += 1;
                    mark[i] = j;
                    i = parent[i];
                }
            }
        }
        let mut l_start = vec![0; n + 1];
        for k in 0..n {
            l_start[k + 1] = l_start[k] + count[k];
        }
        Symbolic { n, perm, col_start, rows, slot, diag_slot, parent, l_start }
    }

    /// The number of nonzeros of L below the diagonal.
    pub fn nnz_l(&self) -> usize {
        self.l_start[self.n]
    }

    /// Factors the matrix with the pattern's values `values` (in its order,
    /// repeats summed) plus `diag` on the diagonal.
    pub fn factor(&self, values: &[f64], diag: &[f64]) -> Factor {
        let n = self.n;
        let mut a = vec![0.0; self.rows.len()];
        for (e, &v) in values.iter().enumerate() {
            a[self.slot[e]] += v;
        }
        for (i, &v) in diag.iter().enumerate() {
            a[self.diag_slot[i]] += v;
        }
        let scale = a.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let tiny = ZERO_PIVOT * scale.max(f64::MIN_POSITIVE);
        let nnz = self.nnz_l();
        let mut l_rows = vec![0; nnz];
        let mut l_values = vec![0.0; nnz];
        let mut d_inv = vec![0.0; n];
        let mut inertia = Inertia { positive: 0, negative: 0, zero: 0 };
        // Up-looking (QDLDL_factor): row k of L solves L[..k, ..k] y = A[..k, k].
        let mut next = self.l_start[..n].to_vec();
        let mut y = vec![0.0; n];
        let mut used = vec![false; n];
        let mut pattern: Vec<usize> = Vec::with_capacity(n);
        let mut stack: Vec<usize> = Vec::with_capacity(n);
        for k in 0..n {
            pattern.clear();
            let mut d = 0.0;
            for p in self.col_start[k]..self.col_start[k + 1] {
                let i = self.rows[p];
                if i == k {
                    d = a[p];
                    continue;
                }
                y[i] = a[p];
                // The path up the tree from i, while unvisited and below k:
                // the nonzeros of row k of L, in topological order.
                if !used[i] {
                    stack.clear();
                    let mut j = i;
                    while j != NONE && j < k && !used[j] {
                        used[j] = true;
                        stack.push(j);
                        j = self.parent[j];
                    }
                    while let Some(j) = stack.pop() {
                        pattern.push(j);
                    }
                }
            }
            for &c in pattern.iter().rev() {
                let yc = y[c];
                let end = next[c];
                for q in self.l_start[c]..end {
                    y[l_rows[q]] -= l_values[q] * yc;
                }
                let lkc = yc * d_inv[c];
                l_rows[end] = k;
                l_values[end] = lkc;
                d -= yc * lkc;
                next[c] += 1;
                y[c] = 0.0;
                used[c] = false;
            }
            // (NaN counts as zero too.)
            if d.is_nan() || d.abs() <= tiny {
                inertia.zero += 1;
                return Factor { l_rows, l_values, d_inv, inertia, complete: false };
            }
            if d > 0.0 {
                inertia.positive += 1;
            } else {
                inertia.negative += 1;
            }
            d_inv[k] = 1.0 / d;
        }
        Factor { l_rows, l_values, d_inv, inertia, complete: true }
    }
}

impl Factor {
    /// Solves `A x = b` in place.
    pub fn solve(&self, sym: &Symbolic, b: &mut [f64]) {
        let n = sym.n;
        let mut x: Vec<f64> = (0..n).map(|k| b[sym.perm[k]]).collect();
        for k in 0..n {
            let xk = x[k];
            if xk != 0.0 {
                for q in sym.l_start[k]..sym.l_start[k + 1] {
                    x[self.l_rows[q]] -= self.l_values[q] * xk;
                }
            }
        }
        for k in 0..n {
            x[k] *= self.d_inv[k];
        }
        for k in (0..n).rev() {
            let mut s = x[k];
            for q in sym.l_start[k]..sym.l_start[k + 1] {
                s -= self.l_values[q] * x[self.l_rows[q]];
            }
            x[k] = s;
        }
        for k in 0..n {
            b[sym.perm[k]] = x[k];
        }
    }
}

/// `A x` for the symmetric matrix with lower-triangle pattern `pattern`
/// (repeats summed), values `values`, plus `diag` on the diagonal.
pub fn multiply(pattern: &[(usize, usize)], values: &[f64], diag: &[f64], x: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = diag.iter().zip(x).map(|(d, v)| d * v).collect();
    for (&(i, j), &v) in pattern.iter().zip(values) {
        out[i] += v * x[j];
        if i != j {
            out[j] += v * x[i];
        }
    }
    out
}

/// A minimum degree ordering of the graph with adjacency lists `adj`
/// (sorted, without self loops): repeatedly eliminates a vertex of the
/// smallest degree (the smallest index among ties), joining its neighbors
/// into a clique, which is the fill its elimination causes.
fn minimum_degree(mut adj: Vec<Vec<usize>>) -> Vec<usize> {
    let n = adj.len();
    let mut done = vec![false; n];
    let mut heap: BinaryHeap<Reverse<(usize, usize)>> = (0..n).map(|v| Reverse((adj[v].len(), v))).collect();
    let mut order = Vec::with_capacity(n);
    let mut merged = Vec::new();
    while let Some(Reverse((degree, v))) = heap.pop() {
        if done[v] || degree != adj[v].len() {
            continue;
        }
        done[v] = true;
        order.push(v);
        let neighbors = std::mem::take(&mut adj[v]);
        for &a in &neighbors {
            // adj[a] := (adj[a] ∪ neighbors) \ {a, v}, kept sorted.
            merged.clear();
            let (mut p, mut q) = (0, 0);
            let old = &adj[a];
            while p < old.len() || q < neighbors.len() {
                let take_old = q == neighbors.len() || (p < old.len() && old[p] <= neighbors[q]);
                let w = if take_old { old[p] } else { neighbors[q] };
                if take_old {
                    if q < neighbors.len() && neighbors[q] == w {
                        q += 1;
                    }
                    p += 1;
                } else {
                    q += 1;
                }
                if w != a && w != v {
                    merged.push(w);
                }
            }
            std::mem::swap(&mut adj[a], &mut merged);
            heap.push(Reverse((adj[a].len(), a)));
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::super::graph::testing::Rng;
    use super::super::ldl::Ldl;
    use super::*;

    /// A random sparse symmetric matrix: its lower-triangle pattern and
    /// values (with repeats and some diagonals missing), and the dense
    /// lower triangle.
    fn random_matrix(
        rng: &mut Rng,
        n: usize,
        density: f64,
        diag: impl Fn(usize) -> f64,
    ) -> (Vec<(usize, usize)>, Vec<f64>, Vec<f64>) {
        let mut pattern = Vec::new();
        let mut values = Vec::new();
        let mut dense = vec![0.0; n * n];
        for i in 0..n {
            for j in 0..i {
                if rng.real(0.0, 1.0) < density {
                    let v = rng.real(-1.0, 1.0);
                    pattern.push((i, j));
                    values.push(v);
                    dense[i * n + j] += v;
                    if rng.below(4) == 0 {
                        // A repeat, summed.
                        pattern.push((i, j));
                        values.push(0.5);
                        dense[i * n + j] += 0.5;
                    }
                }
            }
            let d = diag(i);
            if d != 0.0 {
                pattern.push((i, i));
                values.push(d);
                dense[i * n + i] += d;
            }
        }
        (pattern, values, dense)
    }

    fn residual(pattern: &[(usize, usize)], values: &[f64], x: &[f64], b: &[f64]) -> f64 {
        let n = b.len();
        let ax = multiply(pattern, values, &vec![0.0; n], x);
        ax.iter().zip(b).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max)
    }

    /// Quasi-definite matrices `[H J^T; J -C]` (H diagonally dominant, C
    /// positive): the factorization exists in any order, its inertia is
    /// (size of H, size of C), and it agrees with the dense one.
    #[test]
    fn quasi_definite_matrices() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for case in 0..200 {
            let nh = 1 + rng.below(30);
            let nc = rng.below(20);
            let n = nh + nc;
            let density = [0.05, 0.2, 0.6][case % 3];
            let (mut pattern, mut values, mut dense) = random_matrix(&mut rng, n, density, |_| 0.0);
            // Make H diagonally dominant and C's block negative definite.
            let mut rowsum = vec![0.0; n];
            for (&(i, j), &v) in pattern.iter().zip(&values) {
                // Within H's block or within C's (not J's).
                if (i < nh) == (j < nh) {
                    rowsum[i] += v.abs();
                    rowsum[j] += v.abs();
                }
            }
            for i in 0..n {
                let d = if i < nh { rowsum[i] + rng.real(0.1, 2.0) } else { -(rowsum[i] + rng.real(0.1, 2.0)) };
                pattern.push((i, i));
                values.push(d);
                dense[i * n + i] += d;
            }
            let sym = Symbolic::new(n, &pattern);
            let f = sym.factor(&values, &vec![0.0; n]);
            assert!(f.complete, "case {case}");
            assert_eq!(f.inertia, Inertia { positive: nh, negative: nc, zero: 0 }, "case {case}");
            assert_eq!(f.inertia, Ldl::factor(n, &dense).inertia, "case {case}");
            let b: Vec<f64> = (0..n).map(|_| rng.real(-1.0, 1.0)).collect();
            let mut x = b.clone();
            f.solve(&sym, &mut x);
            assert!(residual(&pattern, &values, &x, &b) < 1e-9, "case {case}");
        }
    }

    /// Indefinite matrices: whenever the factorization completes, its
    /// inertia is the dense one's (Sylvester), and its solutions solve.
    #[test]
    fn indefinite_matrices() {
        let mut rng = Rng(0xbf58476d1ce4e5b9);
        let mut completed = 0;
        for case in 0..300 {
            let n = 1 + rng.below(25);
            let (pattern, values, dense) =
                random_matrix(&mut rng, n, [0.1, 0.4][case % 2], |_| if case % 5 == 0 { 0.0 } else { 3.0 });
            let mut extra = vec![0.0; n];
            for e in extra.iter_mut() {
                *e = rng.real(-4.0, 4.0);
            }
            let mut full = dense.clone();
            for i in 0..n {
                full[i * n + i] += extra[i];
            }
            let sym = Symbolic::new(n, &pattern);
            let f = sym.factor(&values, &extra);
            if !f.complete {
                continue;
            }
            completed += 1;
            let reference = Ldl::factor(n, &full).inertia;
            if reference.zero == 0 {
                assert_eq!(f.inertia, reference, "case {case}");
            }
            let b: Vec<f64> = (0..n).map(|_| rng.real(-1.0, 1.0)).collect();
            let mut x = b.clone();
            f.solve(&sym, &mut x);
            let ax = multiply(&pattern, &values, &extra, &x);
            let r = ax.iter().zip(&b).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            let size = x.iter().fold(1.0f64, |m, v| m.max(v.abs()));
            assert!(r <= 1e-8 * size, "case {case}: residual {r}, |x| {size}");
        }
        assert!(completed > 200, "{completed}");
    }

    /// A zero pivot is reported as a zero eigenvalue.
    #[test]
    fn zero_pivots() {
        // [0 1; 1 0]: the first pivot is 0 in either order.
        let sym = Symbolic::new(2, &[(1, 0)]);
        let f = sym.factor(&[1.0], &[0.0, 0.0]);
        assert!(!f.complete);
        assert_eq!(f.inertia.zero, 1);
        // With -C on the second diagonal it is quasi-definite... only if
        // the first diagonal is positive: [1 1; 1 -1].
        let f = sym.factor(&[1.0], &[1.0, -1.0]);
        assert!(f.complete);
        assert_eq!(f.inertia, Inertia { positive: 1, negative: 1, zero: 0 });
    }

    /// An arrow matrix whose dense row and column come first: in the given
    /// order L fills completely, but minimum degree eliminates the tips
    /// first, which causes no fill at all.
    #[test]
    fn ordering_avoids_fill() {
        let n = 50;
        let mut pattern: Vec<(usize, usize)> = (1..n).map(|i| (i, 0)).collect();
        pattern.extend((0..n).map(|i| (i, i)));
        let sym = Symbolic::new(n, &pattern);
        assert_eq!(sym.nnz_l(), n - 1);
        let mut values = vec![1.0; n - 1];
        values.extend((0..n).map(|i| if i == 0 { n as f64 } else { 2.0 }));
        let f = sym.factor(&values, &vec![0.0; n]);
        assert_eq!(f.inertia, Inertia { positive: n, negative: 0, zero: 0 });
        let b: Vec<f64> = (0..n).map(|i| i as f64).collect();
        let mut x = b.clone();
        f.solve(&sym, &mut x);
        assert!(residual(&pattern, &values, &x, &b) < 1e-12);
    }
}
