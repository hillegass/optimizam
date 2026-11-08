//! Basis matrix factorization, after HiGHS's `HFactor` (highs/util/HFactor.cpp).
//!
//! `build` computes an LU factorization of the basis matrix B by Gaussian
//! elimination: column singletons (including unit logical columns) first, then
//! row singletons, then the remaining kernel with Markowitz pivoting under a
//! threshold rule (`kDefaultPivotThreshold = 0.1`, `kDefaultPivotTolerance =
//! 1e-10`). As in HFactor, the pivot row of each column becomes that column's
//! basis position, so FTRAN results are indexed by row.
//!
//! Basis changes are applied as product-form updates (HFactor's
//! `kUpdateMethodPf`: `updatePF`, `ftranPF`, `btranPF`) until the next
//! refactorization.
//!
//! L is stored as column etas in pivot order; U row-wise in pivot order, with
//! entries indexed by the position (pivot row) of their column.

pub const PIVOT_THRESHOLD: f64 = 0.1;
pub const PIVOT_TOLERANCE: f64 = 1e-10;
const TINY: f64 = 1e-14; // kHighsTiny
const MARKOWITZ_SEARCH_LIMIT: usize = 8;

pub struct Factor {
    m: usize,
    // L etas: for pivot k, rows l_index[l_start[k]..l_start[k+1]] get -= mult * x[l_pivot[k]]
    l_pivot: Vec<usize>,
    l_start: Vec<usize>,
    l_index: Vec<usize>,
    l_value: Vec<f64>,
    // U rows in pivot order
    u_pivot: Vec<usize>,
    u_pivot_value: Vec<f64>,
    u_start: Vec<usize>,
    u_index: Vec<usize>,
    u_value: Vec<f64>,
    // Product-form updates
    pf_pivot: Vec<usize>,
    pf_pivot_value: Vec<f64>,
    pf_start: Vec<usize>,
    pf_index: Vec<usize>,
    pf_value: Vec<f64>,
}

/// Result of a factorization. `position[j]` is the new basis position of
/// input column j. Columns that could not be pivoted are listed with the rows
/// left unpivoted; the caller replaces them with logicals (as HFactor's
/// `buildHandleRankDeficiency` does) and refactors.
pub struct Built {
    pub factor: Factor,
    pub position: Vec<usize>,
    pub singular_cols: Vec<usize>,
    pub unpivoted_rows: Vec<usize>,
}

impl Factor {
    pub fn num_updates(&self) -> usize {
        self.pf_pivot.len()
    }

    /// Factorizes the m x m matrix whose column j has entries
    /// `col_index[col_start[j]..col_start[j + 1]]` (rows) with values
    /// `col_value[..]`.
    pub fn build(m: usize, col_start: &[usize], col_index: &[usize], col_value: &[f64], threshold: f64) -> Built {
        assert_eq!(col_start.len(), m + 1);
        // The active submatrix: columns (rows and values) and rows (columns)
        // in flat arenas, each with some spare space, moved to the end of the
        // arena when it overflows (HFactor's MC and MR storage).
        let mut kernel = Kernel::new(m, col_start, col_index, col_value);
        let mut col_active = vec![true; m];
        let mut row_active = vec![true; m];
        let mut col_singletons: Vec<usize> = (0..m).filter(|&j| kernel.col_count[j] == 1).collect();
        let mut row_singletons: Vec<usize> = (0..m).filter(|&i| kernel.row_count[i] == 1).collect();
        // Active columns by entry count, for the Markowitz search. A column
        // is pushed to its new bucket whenever its count may have changed, so
        // buckets hold stale entries, which the search skips.
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); m + 1];
        for j in 0..m {
            buckets[kernel.col_count[j]].push(j);
        }

        let mut f = Factor {
            m,
            l_pivot: Vec::with_capacity(m),
            l_start: Vec::with_capacity(m + 1),
            l_index: Vec::new(),
            l_value: Vec::new(),
            u_pivot: Vec::with_capacity(m),
            u_pivot_value: Vec::with_capacity(m),
            u_start: Vec::with_capacity(m + 1),
            u_index: Vec::new(),
            u_value: Vec::new(),
            pf_pivot: Vec::new(),
            pf_pivot_value: Vec::new(),
            pf_start: vec![0],
            pf_index: Vec::new(),
            pf_value: Vec::new(),
        };
        f.l_start.push(0);
        f.u_start.push(0);
        let mut position = vec![usize::MAX; m];
        let mut singular_cols = Vec::new();
        let mut marker = vec![usize::MAX; m]; // row -> index in the column being updated
        let mut u_cols: Vec<usize> = Vec::new(); // U entries by column id, translated at the end
        let mut l_entries: Vec<(usize, f64)> = Vec::new();
        let mut u_row: Vec<(usize, f64)> = Vec::new();

        let mut remaining = m;
        while remaining > 0 {
            // 1. Choose a pivot (r, c).
            let mut pivot: Option<(usize, usize)> = None;
            while let Some(j) = col_singletons.pop() {
                if col_active[j] && kernel.col_count[j] == 1 {
                    let (i, v) = kernel.col(j)[0];
                    if v.abs() >= PIVOT_TOLERANCE {
                        pivot = Some((i, j));
                        break;
                    }
                }
            }
            if pivot.is_none() {
                while let Some(i) = row_singletons.pop() {
                    if row_active[i] && kernel.row_count[i] == 1 {
                        let j = kernel.row(i)[0];
                        let cmax = kernel.col(j).iter().fold(0.0f64, |a, (_, v)| a.max(v.abs()));
                        let v = kernel.col(j).iter().find(|(r, _)| *r == i).map_or(0.0, |e| e.1);
                        if v.abs() >= PIVOT_TOLERANCE && v.abs() >= threshold * cmax {
                            pivot = Some((i, j));
                            break;
                        }
                    }
                }
            }
            if pivot.is_none() {
                // Markowitz: search the active columns with the fewest entries,
                // merit (col_count - 1) * (row_count - 1), as in buildKernel.
                let mut best: Option<(f64, usize, usize)> = None;
                let mut searched = 0;
                'buckets: for (count, slot) in buckets.iter_mut().enumerate() {
                    if slot.is_empty() {
                        continue;
                    }
                    let mut bucket = std::mem::take(slot);
                    bucket.retain(|&j| col_active[j] && kernel.col_count[j] == count);
                    bucket.sort_unstable();
                    bucket.dedup();
                    for idx in 0..bucket.len() {
                        if searched >= MARKOWITZ_SEARCH_LIMIT && best.is_some() {
                            *slot = bucket;
                            break 'buckets;
                        }
                        let j = bucket[idx];
                        let cmax = kernel.col(j).iter().fold(0.0f64, |a, (_, v)| a.max(v.abs()));
                        if cmax < PIVOT_TOLERANCE {
                            // Numerically empty column: rank deficient.
                            col_active[j] = false;
                            singular_cols.push(j);
                            for k in 0..kernel.col_count[j] {
                                let i = kernel.col(j)[k].0;
                                kernel.remove_from_row(i, j);
                            }
                            kernel.col_count[j] = 0;
                            remaining -= 1;
                            continue;
                        }
                        searched += 1;
                        for &(i, v) in kernel.col(j) {
                            if v.abs() >= threshold * cmax {
                                let merit = (count as f64 - 1.0) * (kernel.row_count[i] as f64 - 1.0);
                                if best.is_none_or(|(b, _, _)| merit < b) {
                                    best = Some((merit, i, j));
                                }
                            }
                        }
                    }
                    *slot = bucket;
                }
                match best {
                    Some((_, i, j)) => pivot = Some((i, j)),
                    None => break,
                }
            }
            let (r, c) = pivot.unwrap();

            // 2. Eliminate.
            let piv = kernel.col(c).iter().find(|(i, _)| *i == r).unwrap().1;
            // L eta: other rows of the pivot column
            l_entries.clear();
            l_entries.extend(kernel.col(c).iter().filter(|(i, _)| *i != r).map(|&(i, v)| (i, v / piv)));
            // U row: other columns of the pivot row
            u_row.clear();
            for k in 0..kernel.row_count[r] {
                let j = kernel.row(r)[k];
                if j == c {
                    continue;
                }
                let v = kernel.remove_from_col(j, r);
                u_row.push((j, v));
            }
            // Pivot column leaves the active matrix
            for &(i, _) in &l_entries {
                kernel.remove_from_row(i, c);
            }
            col_active[c] = false;
            row_active[r] = false;
            kernel.row_count[r] = 0;
            // Schur complement update
            if !l_entries.is_empty() {
                for &(j, urv) in &u_row {
                    for (k, &(i, _)) in kernel.col(j).iter().enumerate() {
                        marker[i] = k;
                    }
                    for &(i, mult) in &l_entries {
                        let delta = -mult * urv;
                        let k = marker[i];
                        if k < kernel.col_count[j] && kernel.col(j)[k].0 == i {
                            kernel.col_mut(j)[k].1 += delta;
                        } else {
                            marker[i] = kernel.col_count[j];
                            kernel.push_col(j, i, delta);
                            kernel.push_row(i, j);
                        }
                    }
                    // Drop cancelled entries
                    let mut k = 0;
                    while k < kernel.col_count[j] {
                        let (i, v) = kernel.col(j)[k];
                        if v.abs() <= TINY {
                            kernel.swap_remove_col(j, k);
                            kernel.remove_from_row(i, j);
                            if kernel.row_count[i] == 1 {
                                row_singletons.push(i);
                            }
                        } else {
                            k += 1;
                        }
                    }
                    for &(i, _) in kernel.col(j) {
                        marker[i] = usize::MAX;
                    }
                }
            }
            for &(j, _) in &u_row {
                if kernel.col_count[j] == 1 {
                    col_singletons.push(j);
                }
                buckets[kernel.col_count[j]].push(j);
            }
            for &(i, _) in &l_entries {
                if kernel.row_count[i] == 1 {
                    row_singletons.push(i);
                }
            }
            kernel.col_count[c] = 0;

            // 3. Record.
            f.l_pivot.push(r);
            for &(i, mult) in &l_entries {
                f.l_index.push(i);
                f.l_value.push(mult);
            }
            f.l_start.push(f.l_index.len());
            f.u_pivot.push(r);
            f.u_pivot_value.push(piv);
            for &(j, v) in &u_row {
                u_cols.push(j);
                f.u_value.push(v);
            }
            f.u_start.push(u_cols.len());
            position[c] = r;
            remaining -= 1;
        }
        f.u_index = u_cols.iter().map(|&j| position[j]).collect();
        let unpivoted_rows: Vec<usize> = (0..m).filter(|&i| row_active[i]).collect();
        debug_assert_eq!(unpivoted_rows.len(), singular_cols.len());
        if !singular_cols.is_empty() {
            // U entries in singular columns refer to no position; they are
            // only valid after the caller repairs the basis and refactors.
            for (k, &j) in u_cols.iter().enumerate() {
                if position[j] == usize::MAX {
                    f.u_index[k] = 0;
                    f.u_value[k] = 0.0;
                }
            }
        }
        Built { factor: f, position, singular_cols, unpivoted_rows }
    }

    /// Solves B x = b in place. `b` is indexed by row; `x` by basis position.
    pub fn ftran(&self, b: &mut [f64]) {
        // L
        for k in 0..self.l_pivot.len() {
            let pm = b[self.l_pivot[k]];
            if pm != 0.0 {
                for p in self.l_start[k]..self.l_start[k + 1] {
                    b[self.l_index[p]] -= self.l_value[p] * pm;
                }
            }
        }
        // U, backwards in pivot order
        for k in (0..self.u_pivot.len()).rev() {
            let r = self.u_pivot[k];
            let mut v = b[r];
            for p in self.u_start[k]..self.u_start[k + 1] {
                v -= self.u_value[p] * b[self.u_index[p]];
            }
            b[r] = v / self.u_pivot_value[k];
        }
        // PF updates, forwards (ftranPF)
        for k in 0..self.pf_pivot.len() {
            let r = self.pf_pivot[k];
            let mut pm = b[r];
            if pm.abs() > TINY {
                pm /= self.pf_pivot_value[k];
                b[r] = pm;
                for p in self.pf_start[k]..self.pf_start[k + 1] {
                    b[self.pf_index[p]] -= pm * self.pf_value[p];
                }
            }
        }
        clean(b);
    }

    /// Solves B^T y = d in place. `d` is indexed by basis position; `y` by row.
    pub fn btran(&self, d: &mut [f64]) {
        // PF updates, backwards (btranPF)
        for k in (0..self.pf_pivot.len()).rev() {
            let r = self.pf_pivot[k];
            let mut pm = d[r];
            for p in self.pf_start[k]..self.pf_start[k + 1] {
                pm -= self.pf_value[p] * d[self.pf_index[p]];
            }
            d[r] = pm / self.pf_pivot_value[k];
        }
        // U^T, forwards in pivot order
        for k in 0..self.u_pivot.len() {
            let r = self.u_pivot[k];
            let w = d[r] / self.u_pivot_value[k];
            d[r] = w;
            if w != 0.0 {
                for p in self.u_start[k]..self.u_start[k + 1] {
                    d[self.u_index[p]] -= self.u_value[p] * w;
                }
            }
        }
        // L^T, backwards
        for k in (0..self.l_pivot.len()).rev() {
            let r = self.l_pivot[k];
            let mut v = d[r];
            for p in self.l_start[k]..self.l_start[k + 1] {
                v -= self.l_value[p] * d[self.l_index[p]];
            }
            d[r] = v;
        }
        clean(d);
    }

    /// Records the basis change at `position` with pivotal column
    /// `aq` = B^{-1} a_q (HFactor::updatePF).
    pub fn update(&mut self, aq: &[f64], position: usize) {
        for (i, &v) in aq.iter().enumerate() {
            if i != position && v != 0.0 {
                self.pf_index.push(i);
                self.pf_value.push(v);
            }
        }
        self.pf_pivot.push(position);
        self.pf_pivot_value.push(aq[position]);
        self.pf_start.push(self.pf_index.len());
    }

    #[allow(dead_code)]
    pub fn dim(&self) -> usize {
        self.m
    }
}

/// The active submatrix during `Factor::build`: each column's (row, value)
/// entries and each row's columns, in flat arenas. A column or row has
/// `space` slots from `start`; when it overflows, it moves to the end of
/// its arena with twice the space (HFactor's MC and MR storage).
struct Kernel {
    col_start: Vec<usize>,
    col_count: Vec<usize>,
    col_space: Vec<usize>,
    mc: Vec<(usize, f64)>,
    row_start: Vec<usize>,
    row_count: Vec<usize>,
    row_space: Vec<usize>,
    mr: Vec<usize>,
}

impl Kernel {
    fn new(m: usize, col_start: &[usize], col_index: &[usize], col_value: &[f64]) -> Self {
        let mut k = Kernel {
            col_start: Vec::with_capacity(m),
            col_count: Vec::with_capacity(m),
            col_space: Vec::with_capacity(m),
            mc: Vec::with_capacity(2 * col_index.len() + m),
            row_start: vec![0; m],
            row_count: vec![0; m],
            row_space: vec![0; m],
            mr: Vec::new(),
        };
        for j in 0..m {
            let start = k.mc.len();
            for p in col_start[j]..col_start[j + 1] {
                if col_value[p].abs() > TINY {
                    k.mc.push((col_index[p], col_value[p]));
                    k.row_count[col_index[p]] += 1;
                }
            }
            let count = k.mc.len() - start;
            let space = 2 * count + 1;
            k.col_start.push(start);
            k.col_count.push(count);
            k.col_space.push(space);
            k.mc.resize(start + space, (0, 0.0));
        }
        let mut total = 0;
        for i in 0..m {
            k.row_start[i] = total;
            k.row_space[i] = 2 * k.row_count[i] + 1;
            total += k.row_space[i];
        }
        k.mr = vec![0; total];
        let mut fill: Vec<usize> = k.row_start.clone();
        for j in 0..m {
            for p in k.col_start[j]..k.col_start[j] + k.col_count[j] {
                let i = k.mc[p].0;
                k.mr[fill[i]] = j;
                fill[i] += 1;
            }
        }
        k
    }

    fn col(&self, j: usize) -> &[(usize, f64)] {
        &self.mc[self.col_start[j]..self.col_start[j] + self.col_count[j]]
    }

    fn col_mut(&mut self, j: usize) -> &mut [(usize, f64)] {
        &mut self.mc[self.col_start[j]..self.col_start[j] + self.col_count[j]]
    }

    fn row(&self, i: usize) -> &[usize] {
        &self.mr[self.row_start[i]..self.row_start[i] + self.row_count[i]]
    }

    fn push_col(&mut self, j: usize, i: usize, v: f64) {
        if self.col_count[j] == self.col_space[j] {
            let (start, count) = (self.col_start[j], self.col_count[j]);
            let new_start = self.mc.len();
            self.mc.extend_from_within(start..start + count);
            self.col_space[j] = 2 * count + 1;
            self.mc.resize(new_start + self.col_space[j], (0, 0.0));
            self.col_start[j] = new_start;
        }
        self.mc[self.col_start[j] + self.col_count[j]] = (i, v);
        self.col_count[j] += 1;
    }

    fn push_row(&mut self, i: usize, j: usize) {
        if self.row_count[i] == self.row_space[i] {
            let (start, count) = (self.row_start[i], self.row_count[i]);
            let new_start = self.mr.len();
            self.mr.extend_from_within(start..start + count);
            self.row_space[i] = 2 * count + 1;
            self.mr.resize(new_start + self.row_space[i], 0);
            self.row_start[i] = new_start;
        }
        self.mr[self.row_start[i] + self.row_count[i]] = j;
        self.row_count[i] += 1;
    }

    /// Removes column j's entry at index k (the last entry takes its place).
    fn swap_remove_col(&mut self, j: usize, k: usize) -> (usize, f64) {
        let start = self.col_start[j];
        let last = start + self.col_count[j] - 1;
        let e = self.mc[start + k];
        self.mc[start + k] = self.mc[last];
        self.col_count[j] -= 1;
        e
    }

    /// Removes row i's entry from column j, returning its value.
    fn remove_from_col(&mut self, j: usize, i: usize) -> f64 {
        let k = self.col(j).iter().position(|e| e.0 == i).unwrap();
        self.swap_remove_col(j, k).1
    }

    /// Removes column j from row i.
    fn remove_from_row(&mut self, i: usize, j: usize) {
        let start = self.row_start[i];
        let k = self.row(i).iter().position(|&x| x == j).unwrap();
        let last = start + self.row_count[i] - 1;
        self.mr[start + k] = self.mr[last];
        self.row_count[i] -= 1;
    }
}

fn clean(v: &mut [f64]) {
    for x in v.iter_mut() {
        if x.abs() < TINY {
            *x = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % 10_000) as f64 / 10_000.0
        }
    }

    fn random_matrix(m: usize, density: f64, rng: &mut Rng) -> Vec<Vec<(usize, f64)>> {
        (0..m)
            .map(|j| {
                let mut c: Vec<(usize, f64)> = Vec::new();
                for i in 0..m {
                    if rng.next() < density {
                        c.push((i, rng.next() * 2.0 - 1.0));
                    }
                }
                // keep it nonsingular with high probability
                if !c.iter().any(|(i, _)| *i == j) {
                    c.push((j, 1.0 + rng.next()));
                }
                c
            })
            .collect()
    }

    fn flat(cols: &[Vec<(usize, f64)>]) -> (Vec<usize>, Vec<usize>, Vec<f64>) {
        let mut start = vec![0];
        let (mut index, mut value) = (Vec::new(), Vec::new());
        for c in cols {
            index.extend(c.iter().map(|e| e.0));
            value.extend(c.iter().map(|e| e.1));
            start.push(index.len());
        }
        (start, index, value)
    }

    fn build(m: usize, cols: &[Vec<(usize, f64)>]) -> Built {
        let (start, index, value) = flat(cols);
        Factor::build(m, &start, &index, &value, PIVOT_THRESHOLD)
    }

    fn mul(cols: &[Vec<(usize, f64)>], x_by_col: &[f64]) -> Vec<f64> {
        let mut b = vec![0.0; cols.len()];
        for (j, c) in cols.iter().enumerate() {
            for &(i, v) in c {
                b[i] += v * x_by_col[j];
            }
        }
        b
    }

    #[test]
    fn ftran_btran_solve() {
        let mut rng = Rng(12345);
        for &(m, density) in &[(1, 1.0), (5, 0.5), (40, 0.1), (120, 0.03)] {
            let cols = random_matrix(m, density, &mut rng);
            let built = build(m, &cols);
            assert!(built.singular_cols.is_empty());
            let f = built.factor;
            // FTRAN: B x = b, x indexed by position
            let x_true: Vec<f64> = (0..m).map(|_| rng.next() - 0.5).collect();
            let mut b = mul(&cols, &x_true);
            f.ftran(&mut b);
            for j in 0..m {
                assert!((b[built.position[j]] - x_true[j]).abs() < 1e-8, "ftran m={m}");
            }
            // BTRAN: B^T y = d, d indexed by position
            let y_true: Vec<f64> = (0..m).map(|_| rng.next() - 0.5).collect();
            let mut d = vec![0.0; m];
            for (j, c) in cols.iter().enumerate() {
                d[built.position[j]] = c.iter().map(|&(i, v)| v * y_true[i]).sum();
            }
            f.btran(&mut d);
            for i in 0..m {
                assert!((d[i] - y_true[i]).abs() < 1e-8, "btran m={m}");
            }
        }
    }

    #[test]
    fn product_form_update() {
        let mut rng = Rng(999);
        let m = 30;
        let mut cols = random_matrix(m, 0.15, &mut rng);
        let built = build(m, &cols);
        let mut f = built.factor;
        // columns by position
        let mut by_pos = vec![Vec::new(); m];
        for j in 0..m {
            by_pos[built.position[j]] = cols[j].clone();
        }
        for step in 0..5 {
            let p = (step * 7) % m;
            let mut new_col: Vec<(usize, f64)> = Vec::new();
            for i in 0..m {
                if rng.next() < 0.3 {
                    new_col.push((i, rng.next() + 0.5));
                }
            }
            let mut aq = vec![0.0; m];
            for &(i, v) in &new_col {
                aq[i] = v;
            }
            f.ftran(&mut aq);
            if aq[p].abs() < 1e-3 {
                continue;
            }
            f.update(&aq, p);
            by_pos[p] = new_col;
        }
        cols.clone_from(&by_pos);
        let x_true: Vec<f64> = (0..m).map(|_| rng.next() - 0.5).collect();
        let mut b = mul(&cols, &x_true);
        f.ftran(&mut b);
        for p in 0..m {
            assert!((b[p] - x_true[p]).abs() < 1e-7);
        }
        let y_true: Vec<f64> = (0..m).map(|_| rng.next() - 0.5).collect();
        let mut d: Vec<f64> = cols.iter().map(|c| c.iter().map(|&(i, v)| v * y_true[i]).sum()).collect();
        f.btran(&mut d);
        for i in 0..m {
            assert!((d[i] - y_true[i]).abs() < 1e-7);
        }
    }

    #[test]
    fn detects_singularity() {
        // two identical columns
        let cols = vec![vec![(0, 1.0), (1, 2.0)], vec![(0, 1.0), (1, 2.0)], vec![(2, 1.0)]];
        let built = build(3, &cols);
        assert_eq!(built.singular_cols.len(), 1);
        assert_eq!(built.unpivoted_rows.len(), 1);
    }
}
