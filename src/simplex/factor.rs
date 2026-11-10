//! Basis matrix factorization, after HiGHS's `HFactor` (highs/util/HFactor.cpp).
//!
//! `build` computes an LU factorization of the basis matrix B by Gaussian
//! elimination: column singletons (including unit logical columns) first, then
//! row singletons, then the remaining kernel with Markowitz pivoting under a
//! threshold rule (`kDefaultPivotThreshold = 0.1`, `kDefaultPivotTolerance =
//! 1e-10`). As in HFactor, the pivot row of each column becomes that column's
//! basis position, so FTRAN results are indexed by row.
//!
//! Basis changes are Forrest–Tomlin updates (HFactor's `kUpdateMethodFt`:
//! `updateFT`, `ftranFT`, `btranFT`). The entering column, transformed by L
//! and the earlier updates (the spike), replaces the leaving column in U and
//! moves to the end of the pivot order; one row eta then eliminates what the
//! leaving position's row has off the diagonal. U stays sparse, where a
//! product-form update would store the whole transformed column.
//!
//! Every solve is a scatter loop that skips zeros, so its cost follows the
//! nonzeros it meets, not the factor's size. For that, L is kept by column
//! (FTRAN) and by row (BTRAN), and U by column (FTRAN) and by row (BTRAN,
//! and to find a column's entries in an update).

pub const PIVOT_THRESHOLD: f64 = 0.1;
pub const PIVOT_TOLERANCE: f64 = 1e-10;
const TINY: f64 = 1e-14; // kHighsTiny
const MARKOWITZ_SEARCH_LIMIT: usize = 8;
/// Free slots after each row of U, for the entries that updates add.
const ROW_SPARE: usize = 5;
/// A deleted entry of the pivot order.
const NONE: u32 = u32::MAX;

pub struct Factor {
    /// L as column etas in pivot order: FTRAN subtracts `value * b[l_pivot[k]]`
    /// from each row of eta k.
    l_pivot: Vec<u32>,
    l: Packed,
    /// L by row: vector k has, for the row pivoted k-th, the pivot row and
    /// value of each eta with an entry in it.
    lr: Packed,
    /// U's pivot rows (which are their columns' positions) in elimination
    /// order. An update deletes one entry and appends its replacement.
    u_pivot: Vec<u32>,
    u_diag: Vec<f64>,
    /// Each position's entry in `u_pivot`.
    u_lookup: Vec<u32>,
    /// U off the diagonal, by column (entries named by row) and by row
    /// (entries named by position), both indexed as `u_pivot` is.
    uc: Slots,
    ur: Slots,
    /// The updates' row etas: FTRAN subtracts `value * b[index]` from
    /// `b[r_pivot[k]]`.
    r_pivot: Vec<u32>,
    r: Packed,
}

/// A basis matrix that `Factor::build` couldn't factorize: the columns it
/// couldn't pivot and the rows left without a pivot, as many of each. The
/// caller replaces the columns with those rows' logicals (as HFactor's
/// `buildHandleRankDeficiency` does) and builds again.
#[derive(Debug)]
pub struct Singular {
    pub cols: Vec<usize>,
    pub rows: Vec<usize>,
}

/// A sparse vector as parallel index and value lists.
#[derive(Default)]
pub struct SparseVec {
    index: Vec<u32>,
    value: Vec<f64>,
}

impl SparseVec {
    /// Becomes the nonzeros of `dense`.
    fn gather(&mut self, dense: &[f64]) {
        self.index.clear();
        self.value.clear();
        for (i, &v) in dense.iter().enumerate() {
            if v != 0.0 {
                self.index.push(i as u32);
                self.value.push(v);
            }
        }
    }

    fn iter(&self) -> impl Iterator<Item = (u32, f64)> + '_ {
        self.index.iter().copied().zip(self.value.iter().copied())
    }
}

impl Factor {
    pub fn num_updates(&self) -> usize {
        self.r_pivot.len()
    }

    /// Factorizes the m x m matrix whose column j has entries
    /// `col_index[col_start[j]..col_start[j + 1]]` (rows) with values
    /// `col_value[..]`. Returns the factorization and each column's basis
    /// position.
    pub fn build(
        m: usize,
        col_start: &[usize],
        col_index: &[usize],
        col_value: &[f64],
        threshold: f64,
    ) -> Result<(Factor, Vec<usize>), Singular> {
        assert_eq!(col_start.len(), m + 1);
        let mut e = Elimination::new(m, Columns { start: col_start, index: col_index, value: col_value });
        e.triangular();
        if e.pivot.len() < m {
            let mut kernel = Kernel::new(&e, threshold);
            while let Some((r, c)) = kernel.choose() {
                kernel.eliminate(r, c, &mut e);
            }
        }
        e.finish()
    }

    /// Solves B x = b in place. `b` is indexed by row; `x` by basis position.
    pub fn ftran(&self, b: &mut [f64]) {
        self.ftran_lower(b);
        self.ftran_upper(b);
    }

    /// `ftran` for the column entering the basis, which also leaves the
    /// spike in `spike` for `update`.
    pub fn ftran_entering(&self, b: &mut [f64], spike: &mut SparseVec) {
        self.ftran_lower(b);
        spike.gather(b);
        self.ftran_upper(b);
    }

    /// Solves B^T y = d in place. `d` is indexed by basis position; `y` by row.
    pub fn btran(&self, d: &mut [f64]) {
        self.btran_upper(d, 0);
        self.btran_lower(d);
    }

    /// `btran` of the unit vector of `position`, the one leaving the basis:
    /// sets `d` to row `position` of the basis inverse, and leaves in
    /// `partial` what `update` needs. `d` must be zero.
    pub fn btran_leaving(&self, position: usize, d: &mut [f64], partial: &mut SparseVec) {
        d[position] = 1.0;
        // Pivots before this position's leave zeros as they are.
        self.btran_upper(d, self.u_lookup[position] as usize);
        partial.gather(d);
        self.btran_lower(d);
    }

    /// L, then the updates' row etas (HFactor::ftranL, ftranFT).
    fn ftran_lower(&self, b: &mut [f64]) {
        for (k, &r) in self.l_pivot.iter().enumerate() {
            let x = b[r as usize];
            if x.abs() > TINY {
                let (index, value) = self.l.vector(k);
                for (&i, &v) in index.iter().zip(value) {
                    b[i as usize] -= v * x;
                }
            } else {
                b[r as usize] = 0.0;
            }
        }
        for (k, &r) in self.r_pivot.iter().enumerate() {
            let (index, value) = self.r.vector(k);
            let mut x = b[r as usize];
            for (&i, &v) in index.iter().zip(value) {
                x -= v * b[i as usize];
            }
            b[r as usize] = if x.abs() < TINY { 0.0 } else { x };
        }
    }

    /// U, backwards in pivot order (HFactor::ftranU).
    fn ftran_upper(&self, b: &mut [f64]) {
        for (k, &r) in self.u_pivot.iter().enumerate().rev() {
            if r == NONE {
                continue;
            }
            let x = b[r as usize];
            if x.abs() > TINY {
                let x = x / self.u_diag[k];
                b[r as usize] = x;
                let (index, value) = self.uc.vector(k);
                for (&i, &v) in index.iter().zip(value) {
                    b[i as usize] -= v * x;
                }
            } else {
                b[r as usize] = 0.0;
            }
        }
    }

    /// U^T, forwards in pivot order from entry `from` (HFactor::btranU).
    fn btran_upper(&self, d: &mut [f64], from: usize) {
        for (k, &r) in self.u_pivot.iter().enumerate().skip(from) {
            if r == NONE {
                continue;
            }
            let x = d[r as usize];
            if x.abs() > TINY {
                let x = x / self.u_diag[k];
                d[r as usize] = x;
                let (index, value) = self.ur.vector(k);
                for (&i, &v) in index.iter().zip(value) {
                    d[i as usize] -= v * x;
                }
            } else {
                d[r as usize] = 0.0;
            }
        }
    }

    /// The updates' row etas backwards, then L^T (HFactor::btranFT, btranL).
    fn btran_lower(&self, d: &mut [f64]) {
        for (k, &r) in self.r_pivot.iter().enumerate().rev() {
            let x = d[r as usize];
            if x != 0.0 {
                let (index, value) = self.r.vector(k);
                for (&i, &v) in index.iter().zip(value) {
                    d[i as usize] -= v * x;
                }
            }
        }
        for (k, &r) in self.l_pivot.iter().enumerate().rev() {
            let x = d[r as usize];
            if x.abs() > TINY {
                let (index, value) = self.lr.vector(k);
                for (&i, &v) in index.iter().zip(value) {
                    d[i as usize] -= v * x;
                }
            } else {
                d[r as usize] = 0.0;
            }
        }
    }

    /// Records the basis change at `position` (HFactor::updateFT). `spike`
    /// comes from `ftran_entering` of the entering column, `alpha` is that
    /// solve's result at `position`, and `partial` comes from
    /// `btran_leaving(position)`; no other update may come between them.
    pub fn update(&mut self, spike: &SparseVec, partial: &SparseVec, position: usize, alpha: f64) {
        let p = position as u32;
        let old = self.u_lookup[position] as usize;
        let old_diag = self.u_diag[old];
        // Row p and column p leave U.
        for q in self.ur.range(old) {
            let column = self.u_lookup[self.ur.index[q] as usize] as usize;
            self.uc.remove(column, p);
        }
        for q in self.uc.range(old) {
            let row = self.u_lookup[self.uc.index[q] as usize] as usize;
            self.ur.remove(row, p);
        }
        self.u_pivot[old] = NONE;
        self.uc.len[old] = 0;
        self.ur.len[old] = 0;
        // The spike is the new column p, last in the pivot order, so its
        // entries may be in any row.
        let new = self.u_pivot.len();
        self.uc.add(spike.index.len());
        self.ur.add(0);
        for (i, v) in spike.iter().filter(|&(i, _)| i != p) {
            self.uc.push(new, i, v);
            self.ur.push(self.u_lookup[i as usize] as usize, p, v);
        }
        self.u_pivot.push(p);
        self.u_diag.push(old_diag * alpha);
        self.u_lookup[position] = new as u32;
        // The row eta that eliminated row p's entries off the diagonal.
        self.r_pivot.push(p);
        for (i, v) in partial.iter().filter(|&(i, _)| i != p) {
            self.r.push(i, -v * old_diag);
        }
        self.r.close();
    }
}

/// Sparse vectors stored end to end: vector k has the entries
/// `start[k]..start[k + 1]`.
struct Packed {
    start: Vec<usize>,
    index: Vec<u32>,
    value: Vec<f64>,
}

impl Packed {
    fn with_capacity(vectors: usize) -> Packed {
        let mut start = Vec::with_capacity(vectors + 1);
        start.push(0);
        Packed { start, index: Vec::new(), value: Vec::new() }
    }

    /// Adds an entry to the vector being written.
    fn push(&mut self, i: u32, v: f64) {
        self.index.push(i);
        self.value.push(v);
    }

    /// Ends the vector being written.
    fn close(&mut self) {
        self.start.push(self.index.len());
    }

    #[inline(always)]
    fn vector(&self, k: usize) -> (&[u32], &[f64]) {
        let (s, e) = (self.start[k], self.start[k + 1]);
        (&self.index[s..e], &self.value[s..e])
    }

    /// The same entries grouped the other way. Vector k here is named
    /// `pivot[k]` there, and an entry with index i goes to vector `lookup[i]`:
    /// with these, L's etas become L by row, and U's rows U by column.
    fn regrouped(&self, pivot: &[u32], lookup: &[u32]) -> Packed {
        let mut start = vec![0usize; pivot.len() + 1];
        for &i in &self.index {
            start[lookup[i as usize] as usize + 1] += 1;
        }
        for k in 0..pivot.len() {
            start[k + 1] += start[k];
        }
        let mut next = start.clone();
        let mut index = vec![0u32; self.index.len()];
        let mut value = vec![0.0; self.index.len()];
        for (k, &name) in pivot.iter().enumerate() {
            let (is, vs) = self.vector(k);
            for (&i, &v) in is.iter().zip(vs) {
                let slot = &mut next[lookup[i as usize] as usize];
                index[*slot] = name;
                value[*slot] = v;
                *slot += 1;
            }
        }
        Packed { start, index, value }
    }
}

/// Sparse vectors that gain and lose entries. Each has `cap` slots from
/// `start`, and moves to the end of the arena when it outgrows them
/// (HFactor's UR storage).
struct Slots {
    start: Vec<usize>,
    len: Vec<usize>,
    cap: Vec<usize>,
    index: Vec<u32>,
    value: Vec<f64>,
}

impl Slots {
    /// The vectors of `packed`, each with `spare` free slots after it.
    fn new(packed: &Packed, spare: usize) -> Slots {
        let n = packed.start.len() - 1;
        let total = packed.index.len() + n * spare;
        let mut s = Slots {
            start: Vec::with_capacity(n),
            len: Vec::with_capacity(n),
            cap: Vec::with_capacity(n),
            index: Vec::with_capacity(total),
            value: Vec::with_capacity(total),
        };
        for k in 0..n {
            let (index, value) = packed.vector(k);
            s.start.push(s.index.len());
            s.len.push(index.len());
            s.cap.push(index.len() + spare);
            s.index.extend_from_slice(index);
            s.value.extend_from_slice(value);
            s.index.resize(s.index.len() + spare, 0);
            s.value.resize(s.value.len() + spare, 0.0);
        }
        s
    }

    #[inline(always)]
    fn range(&self, k: usize) -> std::ops::Range<usize> {
        self.start[k]..self.start[k] + self.len[k]
    }

    #[inline(always)]
    fn vector(&self, k: usize) -> (&[u32], &[f64]) {
        let r = self.range(k);
        (&self.index[r.clone()], &self.value[r])
    }

    /// Adds an empty vector with `cap` slots.
    fn add(&mut self, cap: usize) {
        self.start.push(self.index.len());
        self.len.push(0);
        self.cap.push(cap);
        self.index.resize(self.index.len() + cap, 0);
        self.value.resize(self.value.len() + cap, 0.0);
    }

    fn push(&mut self, k: usize, i: u32, v: f64) {
        if self.len[k] == self.cap[k] {
            let old = self.range(k);
            self.start[k] = self.index.len();
            self.cap[k] = 2 * self.len[k] + ROW_SPARE;
            self.index.extend_from_within(old.clone());
            self.value.extend_from_within(old);
            self.index.resize(self.start[k] + self.cap[k], 0);
            self.value.resize(self.start[k] + self.cap[k], 0.0);
        }
        let slot = self.start[k] + self.len[k];
        self.index[slot] = i;
        self.value[slot] = v;
        self.len[k] += 1;
    }

    /// Removes vector k's entry with index `i` (the last takes its place).
    fn remove(&mut self, k: usize, i: u32) {
        let r = self.range(k);
        let last = r.end - 1;
        let at = r.start + self.index[r].iter().position(|&x| x == i).expect("U's two copies agree");
        self.index[at] = self.index[last];
        self.value[at] = self.value[last];
        self.len[k] -= 1;
    }
}

/// The matrix to factorize, by column.
#[derive(Clone, Copy)]
struct Columns<'a> {
    start: &'a [usize],
    index: &'a [usize],
    value: &'a [f64],
}

impl<'a> Columns<'a> {
    /// Column j's entries, as (row, value).
    fn column(self, j: usize) -> impl Iterator<Item = (usize, f64)> + 'a {
        let range = self.start[j]..self.start[j + 1];
        self.index[range.clone()].iter().copied().zip(self.value[range].iter().copied()).filter(|e| e.1.abs() > TINY)
    }
}

/// Gaussian elimination of a basis matrix: the pivots made so far, with L's
/// etas and U's columns in pivot order.
struct Elimination<'a> {
    m: usize,
    a: Columns<'a>,
    /// Rows and columns that no pivot has taken (or, in a singular matrix,
    /// given up on).
    row_active: Vec<bool>,
    col_active: Vec<bool>,
    /// The pivot rows, in order.
    pivot: Vec<u32>,
    /// Each pivot's eta, its column of U (entries named by row), and its value.
    l: Packed,
    uc: Packed,
    u_diag: Vec<f64>,
    /// Each column's pivot row, which is its basis position.
    position: Vec<usize>,
}

impl<'a> Elimination<'a> {
    fn new(m: usize, a: Columns<'a>) -> Self {
        Elimination {
            m,
            a,
            row_active: vec![true; m],
            col_active: vec![true; m],
            pivot: Vec::with_capacity(m),
            l: Packed::with_capacity(m),
            uc: Packed::with_capacity(m),
            u_diag: Vec::with_capacity(m),
            position: vec![usize::MAX; m],
        }
    }

    /// Pivots on column and row singletons until none is left
    /// (HFactor::buildSimple). No column changes in this pass: a column
    /// singleton has no eta, and a row singleton's row has no other column to
    /// update. So the etas and U's entries come straight from the matrix,
    /// and a basis of logicals and near-triangular columns never needs the
    /// kernel.
    fn triangular(&mut self) {
        let (m, a) = (self.m, self.a);
        // Entries per row and column among the active ones, and each row's columns.
        let mut row_count = vec![0u32; m];
        let mut col_count = vec![0u32; m];
        for (j, count) in col_count.iter_mut().enumerate() {
            for (i, _) in a.column(j) {
                row_count[i] += 1;
                *count += 1;
            }
        }
        let mut row_start = vec![0usize; m + 1];
        for i in 0..m {
            row_start[i + 1] = row_start[i] + row_count[i] as usize;
        }
        let mut next = row_start.clone();
        let mut row_cols = vec![0u32; row_start[m]];
        for j in 0..m {
            for (i, _) in a.column(j) {
                row_cols[next[i]] = j as u32;
                next[i] += 1;
            }
        }
        let mut col_singles: Vec<usize> = (0..m).filter(|&j| col_count[j] == 1).collect();
        let mut row_singles: Vec<usize> = (0..m).filter(|&i| row_count[i] == 1).collect();
        loop {
            if let Some(c) = col_singles.pop() {
                if !self.col_active[c] || col_count[c] != 1 {
                    continue;
                }
                let (r, v) = a.column(c).find(|&(i, _)| self.row_active[i]).expect("the column has an active entry");
                self.col_active[c] = false;
                if v.abs() < PIVOT_TOLERANCE {
                    // Nothing to pivot on: the matrix is singular.
                    row_count[r] -= 1;
                    if row_count[r] == 1 {
                        row_singles.push(r);
                    }
                    continue;
                }
                self.row_active[r] = false;
                for &j in &row_cols[row_start[r]..row_start[r + 1]] {
                    let j = j as usize;
                    if self.col_active[j] {
                        col_count[j] -= 1;
                        if col_count[j] == 1 {
                            col_singles.push(j);
                        }
                    }
                }
                self.record(r, c, v);
            } else if let Some(r) = row_singles.pop() {
                if !self.row_active[r] || row_count[r] != 1 {
                    continue;
                }
                let c = row_cols[row_start[r]..row_start[r + 1]]
                    .iter()
                    .map(|&j| j as usize)
                    .find(|&j| self.col_active[j])
                    .expect("the row has an active entry");
                let v = a.column(c).find(|&(i, _)| i == r).expect("the row lists the column").1;
                // The row is the column's whatever the other entries are: no
                // other column can pivot on it.
                self.row_active[r] = false;
                if v.abs() < PIVOT_TOLERANCE {
                    // The row gets no pivot: the matrix is singular.
                    col_count[c] -= 1;
                    if col_count[c] == 1 {
                        col_singles.push(c);
                    }
                    continue;
                }
                self.col_active[c] = false;
                self.record(r, c, v);
                for (i, _) in a.column(c).filter(|&(i, _)| self.row_active[i]) {
                    row_count[i] -= 1;
                    if row_count[i] == 1 {
                        row_singles.push(i);
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Records the pivot of value `v` at (r, c) on a column the elimination
    /// hasn't changed: its entries in active rows, over the pivot, are the
    /// eta, and the others its column of U.
    fn record(&mut self, r: usize, c: usize, v: f64) {
        for (i, x) in self.a.column(c).filter(|&(i, _)| i != r) {
            if self.row_active[i] {
                self.l.push(i as u32, x / v);
            } else {
                self.uc.push(i as u32, x);
            }
        }
        self.pivoted(r, c, v);
    }

    /// Ends the pivot at (r, c) whose eta and column of U have been pushed.
    fn pivoted(&mut self, r: usize, c: usize, v: f64) {
        self.l.close();
        self.uc.close();
        self.u_diag.push(v);
        self.pivot.push(r as u32);
        self.position[c] = r;
    }

    fn finish(self) -> Result<(Factor, Vec<usize>), Singular> {
        let m = self.m;
        if self.pivot.len() < m {
            let mut pivoted = vec![false; m];
            for &r in &self.pivot {
                pivoted[r as usize] = true;
            }
            return Err(Singular {
                cols: (0..m).filter(|&j| self.position[j] == usize::MAX).collect(),
                rows: (0..m).filter(|&i| !pivoted[i]).collect(),
            });
        }
        let mut u_lookup = vec![NONE; m];
        for (k, &r) in self.pivot.iter().enumerate() {
            u_lookup[r as usize] = k as u32;
        }
        let factor = Factor {
            lr: self.l.regrouped(&self.pivot, &u_lookup),
            l: self.l,
            ur: Slots::new(&self.uc.regrouped(&self.pivot, &u_lookup), ROW_SPARE),
            uc: Slots::new(&self.uc, 0),
            l_pivot: self.pivot.clone(),
            u_pivot: self.pivot,
            u_diag: self.u_diag,
            u_lookup,
            r_pivot: Vec::new(),
            r: Packed::with_capacity(0),
        };
        Ok((factor, self.position))
    }
}

/// The active submatrix that the triangular pass leaves (HFactor's kernel,
/// with its MC and MR storage).
///
/// A column has `c_space` slots from `c_start`. Its entries in active rows
/// fill them from the front; its entries in pivoted rows, which are its part
/// of U, from the back, so an entry changes sides without moving the others.
/// A row lists its active columns. A column or row that outgrows its slots
/// moves to the end of its arena. Columns and rows are also linked in lists
/// by their number of active entries, for the pivot search.
struct Kernel {
    threshold: f64,
    c_start: Vec<usize>,
    c_count: Vec<usize>,
    c_done: Vec<usize>,
    c_space: Vec<usize>,
    c_index: Vec<u32>,
    c_value: Vec<f64>,
    /// The smallest pivot each column accepts: the threshold times its
    /// largest active entry.
    c_min_pivot: Vec<f64>,
    r_start: Vec<usize>,
    r_count: Vec<usize>,
    r_space: Vec<usize>,
    r_index: Vec<u32>,
    cols: CountLists,
    rows: CountLists,
    /// During an elimination: the pivot column's eta, its multipliers by
    /// row, and the rows that have one.
    eta: Vec<(u32, f64)>,
    work: Vec<f64>,
    in_eta: Vec<bool>,
}

impl Kernel {
    fn new(e: &Elimination, threshold: f64) -> Kernel {
        let (m, a) = (e.m, e.a);
        let mut k = Kernel {
            threshold,
            c_start: vec![0; m],
            c_count: vec![0; m],
            c_done: vec![0; m],
            c_space: vec![0; m],
            c_index: Vec::new(),
            c_value: Vec::new(),
            c_min_pivot: vec![0.0; m],
            r_start: vec![0; m],
            r_count: vec![0; m],
            r_space: vec![0; m],
            r_index: Vec::new(),
            cols: CountLists::new(m),
            rows: CountLists::new(m),
            eta: Vec::new(),
            work: vec![0.0; m],
            in_eta: vec![false; m],
        };
        let columns: Vec<usize> = (0..m).filter(|&j| e.col_active[j]).collect();
        for &j in &columns {
            let active = a.column(j).filter(|&(i, _)| e.row_active[i]).count();
            let space = 2 * active + (a.start[j + 1] - a.start[j] - active) + 4;
            let start = k.c_index.len();
            k.c_index.resize(start + space, 0);
            k.c_value.resize(start + space, 0.0);
            (k.c_start[j], k.c_space[j]) = (start, space);
            for (i, v) in a.column(j) {
                let slot = if e.row_active[i] {
                    k.c_count[j] += 1;
                    k.r_count[i] += 1;
                    start + k.c_count[j] - 1
                } else {
                    k.c_done[j] += 1;
                    start + space - k.c_done[j]
                };
                k.c_index[slot] = i as u32;
                k.c_value[slot] = v;
            }
        }
        let rows: Vec<usize> = (0..m).filter(|&i| e.row_active[i]).collect();
        let mut total = 0;
        for &i in &rows {
            k.r_start[i] = total;
            k.r_space[i] = 2 * k.r_count[i] + 4;
            total += k.r_space[i];
        }
        k.r_index = vec![0; total];
        let mut next = k.r_start.clone();
        for &j in &columns {
            for q in k.c_start[j]..k.c_start[j] + k.c_count[j] {
                let i = k.c_index[q] as usize;
                k.r_index[next[i]] = j as u32;
                next[i] += 1;
            }
        }
        for &i in &rows {
            k.rows.add(i, k.r_count[i]);
        }
        for &j in &columns {
            k.link_col(j);
        }
        k
    }

    /// The next pivot (row, column): a singleton if there is one, or else
    /// the entry of least Markowitz merit, (column count - 1) * (row count -
    /// 1), among those the threshold rule accepts in the first few columns
    /// and rows with the fewest entries (HFactor::buildKernel's search).
    fn choose(&mut self) -> Option<(usize, usize)> {
        loop {
            if let Some(c) = self.cols.first(1) {
                return Some((self.c_index[self.c_start[c]] as usize, c));
            }
            let Some(r) = self.rows.first(1) else { break };
            let c = self.r_index[self.r_start[r]] as usize;
            let at = self.find(c, r);
            if self.c_value[at].abs() >= PIVOT_TOLERANCE {
                return Some((r, c));
            }
            // The row's one entry is too small to pivot on, so it gets no
            // pivot: the matrix is singular.
            self.cols.remove(c);
            self.rows.remove(r);
            self.r_count[r] = 0;
            let last = self.c_start[c] + self.c_count[c] - 1;
            self.c_index[at] = self.c_index[last];
            self.c_value[at] = self.c_value[last];
            self.c_count[c] -= 1;
            self.link_col(c);
        }
        let mut best: Option<(usize, usize, usize)> = None; // merit, row, column
        let mut searched = 0;
        for count in 2..self.cols.first.len() {
            let mut link = self.cols.first[count];
            while link != NONE {
                let c = link as usize;
                let mut settled = false;
                for q in self.c_start[c]..self.c_start[c] + count {
                    if self.c_value[q].abs() >= self.c_min_pivot[c] {
                        let r = self.c_index[q] as usize;
                        let merit = (count - 1) * (self.r_count[r] - 1);
                        if best.is_none_or(|b| merit < b.0) {
                            best = Some((merit, r, c));
                            settled |= self.r_count[r] < count;
                        }
                    }
                }
                searched += 1;
                if settled || (searched > MARKOWITZ_SEARCH_LIMIT && best.is_some()) {
                    return best.map(|(_, r, c)| (r, c));
                }
                link = self.cols.next[c];
            }
            let mut link = self.rows.first[count];
            while link != NONE {
                let r = link as usize;
                let mut settled = false;
                for q in self.r_start[r]..self.r_start[r] + count {
                    let c = self.r_index[q] as usize;
                    let merit = (count - 1) * (self.c_count[c] - 1);
                    if best.is_none_or(|b| merit < b.0) && self.c_value[self.find(c, r)].abs() >= self.c_min_pivot[c] {
                        best = Some((merit, r, c));
                        settled |= self.c_count[c] <= count;
                    }
                }
                searched += 1;
                if settled || (searched > MARKOWITZ_SEARCH_LIMIT && best.is_some()) {
                    return best.map(|(_, r, c)| (r, c));
                }
                link = self.rows.next[r];
            }
        }
        best.map(|(_, r, c)| (r, c))
    }

    /// Pivots on (r, c): records the eta and the column of U, moves row r's
    /// other entries to their columns' parts of U, and subtracts the eta's
    /// multiples of them from the rest of those columns.
    fn eliminate(&mut self, r: usize, c: usize, e: &mut Elimination) {
        self.cols.remove(c);
        self.rows.remove(r);
        let (start, count) = (self.c_start[c], self.c_count[c]);
        let at = self.find(c, r);
        let pivot = self.c_value[at];
        let mut eta = std::mem::take(&mut self.eta);
        eta.clear();
        for q in (start..start + count).filter(|&q| q != at) {
            let (i, mult) = (self.c_index[q], self.c_value[q] / pivot);
            e.l.push(i, mult);
            eta.push((i, mult));
            self.work[i as usize] = mult;
            self.in_eta[i as usize] = true;
        }
        let end = start + self.c_space[c];
        for q in end - self.c_done[c]..end {
            e.uc.push(self.c_index[q], self.c_value[q]);
        }
        e.pivoted(r, c, pivot);
        self.c_count[c] = 0;
        for &(i, _) in &eta {
            self.row_remove(i as usize, c);
        }
        // Row r's storage stays where it is while other rows grow.
        let row = self.r_start[r]..self.r_start[r] + self.r_count[r];
        self.r_count[r] = 0;
        for q in row {
            let j = self.r_index[q] as usize;
            if j != c {
                self.cols.remove(j);
                let u = self.retire(j, r);
                self.update_column(j, u, &eta);
                self.link_col(j);
            }
        }
        for &(i, _) in &eta {
            let i = i as usize;
            self.in_eta[i] = false;
            self.rows.remove(i);
            self.rows.add(i, self.r_count[i]);
        }
        self.eta = eta;
    }

    /// Moves column j's entry in row r, now pivoted, to the column's part of
    /// U, and returns its value.
    fn retire(&mut self, j: usize, r: usize) -> f64 {
        let at = self.find(j, r);
        let u = self.c_value[at];
        let last = self.c_start[j] + self.c_count[j] - 1;
        self.c_index[at] = self.c_index[last];
        self.c_value[at] = self.c_value[last];
        self.c_count[j] -= 1;
        self.c_done[j] += 1;
        let slot = self.c_start[j] + self.c_space[j] - self.c_done[j];
        self.c_index[slot] = r as u32;
        self.c_value[slot] = u;
        u
    }

    /// Subtracts `u` times the eta from column j's active entries.
    fn update_column(&mut self, j: usize, u: f64, eta: &[(u32, f64)]) {
        let start = self.c_start[j];
        let mut write = start;
        let mut hits = 0;
        for q in start..start + self.c_count[j] {
            let i = self.c_index[q] as usize;
            let mut v = self.c_value[q];
            if self.in_eta[i] {
                self.in_eta[i] = false;
                hits += 1;
                v -= u * self.work[i];
                if v.abs() < TINY {
                    self.row_remove(i, j);
                    continue;
                }
            }
            self.c_index[write] = i as u32;
            self.c_value[write] = v;
            write += 1;
        }
        self.c_count[j] = write - start;
        // The eta's rows that the column had no entry in fill in.
        if hits < eta.len() {
            for &(i, mult) in eta {
                let v = -u * mult;
                if self.in_eta[i as usize] && v.abs() >= TINY {
                    self.col_push(j, i, v);
                    self.row_push(i as usize, j as u32);
                }
            }
        }
        for &(i, _) in eta {
            self.in_eta[i as usize] = true;
        }
    }

    /// Puts column j, whose active entries have changed, in the list of its
    /// count with its smallest acceptable pivot. A column with nothing to
    /// pivot on (the matrix is singular) leaves its rows instead.
    fn link_col(&mut self, j: usize) {
        let range = self.c_start[j]..self.c_start[j] + self.c_count[j];
        let largest = self.c_value[range.clone()].iter().fold(0.0f64, |a, v| a.max(v.abs()));
        if largest < PIVOT_TOLERANCE {
            for q in range {
                let i = self.c_index[q] as usize;
                self.row_remove(i, j);
                self.rows.remove(i);
                self.rows.add(i, self.r_count[i]);
            }
            self.c_count[j] = 0;
        } else {
            self.c_min_pivot[j] = self.threshold * largest;
            self.cols.add(j, self.c_count[j]);
        }
    }

    /// Where column j's active entry in row r is.
    fn find(&self, j: usize, r: usize) -> usize {
        let start = self.c_start[j];
        let entries = &self.c_index[start..start + self.c_count[j]];
        start + entries.iter().position(|&i| i as usize == r).expect("the row lists the column")
    }

    fn col_push(&mut self, j: usize, i: u32, v: f64) {
        let (start, count, done, space) = (self.c_start[j], self.c_count[j], self.c_done[j], self.c_space[j]);
        if count + done == space {
            let new_start = self.c_index.len();
            let new_space = 2 * space + 4;
            self.c_index.resize(new_start + new_space, 0);
            self.c_value.resize(new_start + new_space, 0.0);
            self.c_index.copy_within(start..start + count, new_start);
            self.c_value.copy_within(start..start + count, new_start);
            self.c_index.copy_within(start + space - done..start + space, new_start + new_space - done);
            self.c_value.copy_within(start + space - done..start + space, new_start + new_space - done);
            (self.c_start[j], self.c_space[j]) = (new_start, new_space);
        }
        let slot = self.c_start[j] + count;
        self.c_index[slot] = i;
        self.c_value[slot] = v;
        self.c_count[j] += 1;
    }

    fn row_push(&mut self, i: usize, j: u32) {
        let (start, count) = (self.r_start[i], self.r_count[i]);
        if count == self.r_space[i] {
            self.r_start[i] = self.r_index.len();
            self.r_space[i] = 2 * count + 4;
            self.r_index.extend_from_within(start..start + count);
            self.r_index.resize(self.r_start[i] + self.r_space[i], 0);
        }
        self.r_index[self.r_start[i] + count] = j;
        self.r_count[i] += 1;
    }

    /// Removes column j from row i's list.
    fn row_remove(&mut self, i: usize, j: usize) {
        let start = self.r_start[i];
        let last = start + self.r_count[i] - 1;
        let at =
            start + self.r_index[start..=last].iter().position(|&x| x as usize == j).expect("the column is in the row");
        self.r_index[at] = self.r_index[last];
        self.r_count[i] -= 1;
    }
}

/// Items in doubly linked lists by a count each has (HFactor's `clink` and
/// `rlink`): moving an item between counts and reading a count's items both
/// take constant time per item.
struct CountLists {
    first: Vec<u32>,
    next: Vec<u32>,
    prev: Vec<u32>,
    /// The list each item is in, if any.
    count: Vec<u32>,
}

impl CountLists {
    /// Lists for `items` items with counts up to `items`.
    fn new(items: usize) -> CountLists {
        CountLists {
            first: vec![NONE; items + 1],
            next: vec![NONE; items],
            prev: vec![NONE; items],
            count: vec![NONE; items],
        }
    }

    fn first(&self, count: usize) -> Option<usize> {
        self.first.get(count).filter(|&&item| item != NONE).map(|&item| item as usize)
    }

    fn add(&mut self, item: usize, count: usize) {
        let head = self.first[count];
        (self.next[item], self.prev[item], self.count[item]) = (head, NONE, count as u32);
        if head != NONE {
            self.prev[head as usize] = item as u32;
        }
        self.first[count] = item as u32;
    }

    /// Takes `item` out of its list, if it is in one.
    fn remove(&mut self, item: usize) {
        let count = self.count[item];
        if count == NONE {
            return;
        }
        let (prev, next) = (self.prev[item], self.next[item]);
        if prev == NONE {
            self.first[count as usize] = next;
        } else {
            self.next[prev as usize] = next;
        }
        if next != NONE {
            self.prev[next as usize] = prev;
        }
        self.count[item] = NONE;
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

    fn build(m: usize, cols: &[Vec<(usize, f64)>]) -> Result<(Factor, Vec<usize>), Singular> {
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

    /// Checks `f` against the matrix whose column at position p is `by_pos[p]`.
    fn check_solves(f: &Factor, by_pos: &[Vec<(usize, f64)>], rng: &mut Rng, tolerance: f64) {
        let m = by_pos.len();
        let x_true: Vec<f64> = (0..m).map(|_| rng.next() - 0.5).collect();
        let mut b = mul(by_pos, &x_true);
        f.ftran(&mut b);
        for p in 0..m {
            assert!((b[p] - x_true[p]).abs() < tolerance, "ftran m={m}: {} vs {}", b[p], x_true[p]);
        }
        let y_true: Vec<f64> = (0..m).map(|_| rng.next() - 0.5).collect();
        let mut d: Vec<f64> = by_pos.iter().map(|c| c.iter().map(|&(i, v)| v * y_true[i]).sum()).collect();
        f.btran(&mut d);
        for i in 0..m {
            assert!((d[i] - y_true[i]).abs() < tolerance, "btran m={m}: {} vs {}", d[i], y_true[i]);
        }
    }

    fn by_position(cols: &[Vec<(usize, f64)>], position: &[usize]) -> Vec<Vec<(usize, f64)>> {
        let mut by_pos = vec![Vec::new(); cols.len()];
        for (j, c) in cols.iter().enumerate() {
            by_pos[position[j]] = c.clone();
        }
        by_pos
    }

    #[test]
    fn ftran_btran_solve() {
        let mut rng = Rng(12345);
        for &(m, density) in &[(1, 1.0), (5, 0.5), (40, 0.1), (120, 0.03)] {
            let cols = random_matrix(m, density, &mut rng);
            let (f, position) = build(m, &cols).unwrap();
            check_solves(&f, &by_position(&cols, &position), &mut rng, 1e-8);
        }
    }

    /// Replaces columns one at a time, as the simplex does, checking the
    /// solves after each: positions repeat, rows of U outgrow their spare
    /// slots, a replaced column is replaced again, and the factorization is
    /// rebuilt now and then (errors build up over the updates).
    #[test]
    fn forrest_tomlin_updates() {
        const REBUILD: usize = 40;
        let mut rng = Rng(999);
        for &(m, density, steps) in &[(30, 0.15, 60), (80, 0.05, 150), (6, 0.6, 20)] {
            let cols = random_matrix(m, density, &mut rng);
            let (mut f, position) = build(m, &cols).unwrap();
            let mut by_pos = by_position(&cols, &position);
            let (mut spike, mut partial) = (SparseVec::default(), SparseVec::default());
            let mut updates = 0;
            for step in 0..steps {
                if f.num_updates() == REBUILD {
                    let (rebuilt, position) = build(m, &by_pos).unwrap();
                    f = rebuilt;
                    by_pos = by_position(&by_pos, &position);
                }
                let p = (step * 7 + step / 3) % m;
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
                f.ftran_entering(&mut aq, &mut spike);
                if aq[p].abs() < 1e-2 {
                    continue;
                }
                let mut ep = vec![0.0; m];
                f.btran_leaving(p, &mut ep, &mut partial);
                // Row p of the inverse times the entering column is the pivot.
                let alpha_row: f64 = new_col.iter().map(|&(i, v)| ep[i] * v).sum();
                assert!((alpha_row - aq[p]).abs() < 1e-6 * aq[p].abs().max(1.0), "m={m} step={step}");
                f.update(&spike, &partial, p, aq[p]);
                by_pos[p] = new_col;
                updates += 1;
                check_solves(&f, &by_pos, &mut rng, 1e-5);
            }
            assert!(updates > steps / 2, "too few updates were tried: {updates}");
        }
    }

    #[test]
    fn detects_singularity() {
        // two identical columns
        let cols = vec![vec![(0, 1.0), (1, 2.0)], vec![(0, 1.0), (1, 2.0)], vec![(2, 1.0)]];
        let Err(singular) = build(3, &cols) else { panic!("factorized a singular matrix") };
        assert_eq!(singular.cols.len(), 1);
        assert_eq!(singular.rows.len(), 1);
    }
}
