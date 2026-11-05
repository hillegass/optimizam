//! Dense symmetric indefinite factorization `P A P^T = L D L^T` with
//! Bunch–Kaufman pivoting (LAPACK's unblocked `sytf2`, lower form), which
//! also gives the inertia of A: the numbers of positive, negative, and zero
//! eigenvalues. D is block diagonal with 1x1 and 2x2 blocks; a 2x2 block
//! chosen by the pivoting has a negative determinant, so it contributes one
//! positive and one negative eigenvalue.

/// Pivots smaller than this, relative to the largest entry of A, count as
/// zero eigenvalues. It is tiny on purpose: KKT matrices mix entries as
/// large as 1e10 (barrier terms near a bound) with regularizations as small
/// as 1e-11, and those must keep their signs. A numerically singular matrix
/// then shows up with tiny pivots of either sign, so the inertia (not a
/// zero count) is what tells the interior point method to regularize.
const ZERO_PIVOT: f64 = 1e-30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inertia {
    pub positive: usize,
    pub negative: usize,
    pub zero: usize,
}

pub struct Ldl {
    n: usize,
    /// L below the diagonal (unit diagonal implied), D's blocks on the
    /// diagonal and first subdiagonal, row-major n x n.
    a: Vec<f64>,
    /// For each step: the row swapped with it (LAPACK's ipiv, 0-based).
    swaps: Vec<usize>,
    /// Whether step k starts a 2x2 block.
    two: Vec<bool>,
    pub inertia: Inertia,
}

impl Ldl {
    /// Factors the symmetric matrix whose lower triangle is given (row-major
    /// n x n; the upper triangle is ignored).
    pub fn factor(n: usize, lower: &[f64]) -> Ldl {
        let mut a = vec![0.0; n * n];
        for i in 0..n {
            for j in 0..=i {
                a[i * n + j] = lower[i * n + j];
                a[j * n + i] = lower[i * n + j];
            }
        }
        let scale = a.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let tiny = ZERO_PIVOT * scale.max(f64::MIN_POSITIVE);
        let alpha = (1.0 + 17f64.sqrt()) / 8.0;
        let mut swaps = vec![0; n];
        let mut two = vec![false; n];
        let mut inertia = Inertia { positive: 0, negative: 0, zero: 0 };
        let at = |i: usize, j: usize| i * n + j;
        let mut k = 0;
        while k < n {
            let absakk = a[at(k, k)].abs();
            let (imax, colmax) =
                (k + 1..n).map(|i| (i, a[at(i, k)].abs())).fold((k, 0.0), |b, c| if c.1 > b.1 { c } else { b });
            if absakk.max(colmax) <= tiny {
                // A zero column: a zero pivot, nothing to eliminate.
                swaps[k] = k;
                inertia.zero += 1;
                for i in k + 1..n {
                    a[at(i, k)] = 0.0;
                }
                k += 1;
                continue;
            }
            let (kp, size) = if absakk >= alpha * colmax {
                (k, 1)
            } else {
                // The largest off-diagonal entry in row/column imax.
                let rowmax =
                    (k..n).filter(|&j| j != imax).map(|j| a[at(imax.max(j), imax.min(j))].abs()).fold(0.0, f64::max);
                if absakk * rowmax >= alpha * colmax * colmax {
                    (k, 1)
                } else if a[at(imax, imax)].abs() >= alpha * rowmax {
                    (imax, 1)
                } else {
                    (imax, 2)
                }
            };
            // Interchange rows and columns kk and kp of the trailing matrix
            // (kk = k for a 1x1 pivot, k + 1 for a 2x2 one). L's earlier
            // columns stay as they are: `solve` applies the interchanges
            // between them, as LAPACK's sytrs does.
            let kk = k + size - 1;
            if kp != kk {
                symmetric_swap(&mut a, n, kk, kp, k);
            }
            swaps[k] = kp;
            if size == 1 {
                let d = a[at(k, k)];
                if d.abs() <= tiny {
                    inertia.zero += 1;
                } else if d > 0.0 {
                    inertia.positive += 1;
                } else {
                    inertia.negative += 1;
                }
                if d != 0.0 {
                    let r = 1.0 / d;
                    for i in k + 1..n {
                        let lik = a[at(i, k)] * r;
                        for j in k + 1..=i {
                            a[at(i, j)] -= lik * a[at(j, k)];
                        }
                    }
                    for i in k + 1..n {
                        a[at(i, k)] *= r;
                    }
                }
                // Keep the trailing matrix symmetric (only its lower triangle was updated).
                for i in k + 1..n {
                    for j in k + 1..i {
                        a[at(j, i)] = a[at(i, j)];
                    }
                }
            } else {
                two[k] = true;
                swaps[k + 1] = kp;
                let (d11, d21, d22) = (a[at(k, k)], a[at(k + 1, k)], a[at(k + 1, k + 1)]);
                let det = d11 * d22 - d21 * d21;
                // Eigenvalue signs of the block.
                let (tr, root) = (d11 + d22, ((d11 - d22).powi(2) + 4.0 * d21 * d21).sqrt());
                for e in [0.5 * (tr + root), 0.5 * (tr - root)] {
                    if e.abs() <= tiny {
                        inertia.zero += 1;
                    } else if e > 0.0 {
                        inertia.positive += 1;
                    } else {
                        inertia.negative += 1;
                    }
                }
                let (i11, i21, i22) = (d22 / det, -d21 / det, d11 / det);
                // The block's columns (before they become L) and L's entries.
                let c1: Vec<f64> = (k + 2..n).map(|i| a[at(i, k)]).collect();
                let c2: Vec<f64> = (k + 2..n).map(|i| a[at(i, k + 1)]).collect();
                let l1: Vec<f64> = c1.iter().zip(&c2).map(|(x, y)| x * i11 + y * i21).collect();
                let l2: Vec<f64> = c1.iter().zip(&c2).map(|(x, y)| x * i21 + y * i22).collect();
                for (r, i) in (k + 2..n).enumerate() {
                    for (c, j) in (k + 2..=i).enumerate() {
                        a[at(i, j)] -= l1[r] * c1[c] + l2[r] * c2[c];
                    }
                    a[at(i, k)] = l1[r];
                    a[at(i, k + 1)] = l2[r];
                }
                for i in k + 2..n {
                    for j in k + 2..i {
                        a[at(j, i)] = a[at(i, j)];
                    }
                }
            }
            k += size;
        }
        Ldl { n, a, swaps, two, inertia }
    }

    /// Solves A x = b in place.
    pub fn solve(&self, b: &mut [f64]) {
        let n = self.n;
        let at = |i: usize, j: usize| i * n + j;
        // Forward: apply the interchanges and L^-1, step by step.
        let mut k = 0;
        while k < n {
            if self.two[k] {
                b.swap(k + 1, self.swaps[k]);
                for i in k + 2..n {
                    b[i] -= self.a[at(i, k)] * b[k] + self.a[at(i, k + 1)] * b[k + 1];
                }
                k += 2;
            } else {
                b.swap(k, self.swaps[k]);
                for i in k + 1..n {
                    b[i] -= self.a[at(i, k)] * b[k];
                }
                k += 1;
            }
        }
        // D^-1.
        let mut k = 0;
        while k < n {
            if self.two[k] {
                let (d11, d21, d22) = (self.a[at(k, k)], self.a[at(k + 1, k)], self.a[at(k + 1, k + 1)]);
                let det = d11 * d22 - d21 * d21;
                let (x, y) = (b[k], b[k + 1]);
                b[k] = (d22 * x - d21 * y) / det;
                b[k + 1] = (d11 * y - d21 * x) / det;
                k += 2;
            } else {
                let d = self.a[at(k, k)];
                b[k] = if d != 0.0 { b[k] / d } else { 0.0 };
                k += 1;
            }
        }
        // Backward: L^-T and the interchanges in reverse.
        let mut steps = Vec::new();
        let mut k = 0;
        while k < n {
            steps.push(k);
            k += if self.two[k] { 2 } else { 1 };
        }
        for &k in steps.iter().rev() {
            if self.two[k] {
                for i in k + 2..n {
                    b[k] -= self.a[at(i, k)] * b[i];
                    b[k + 1] -= self.a[at(i, k + 1)] * b[i];
                }
                b.swap(k + 1, self.swaps[k]);
            } else {
                for i in k + 1..n {
                    b[k] -= self.a[at(i, k)] * b[i];
                }
                b.swap(k, self.swaps[k]);
            }
        }
    }
}

/// Swaps rows and columns p and q of the symmetric trailing matrix that
/// starts at row/column k.
fn symmetric_swap(a: &mut [f64], n: usize, p: usize, q: usize, k: usize) {
    for j in k..n {
        a.swap(p * n + j, q * n + j);
    }
    for i in k..n {
        a.swap(i * n + p, i * n + q);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn real(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % 2_000_001) as f64 / 1e6 - 1.0
        }
    }

    /// A random symmetric matrix Q diag(eigs) Q^T with Q orthogonal (from
    /// Householder reflections).
    fn with_eigenvalues(rng: &mut Rng, eigs: &[f64]) -> Vec<f64> {
        let n = eigs.len();
        let mut q = vec![0.0; n * n];
        for i in 0..n {
            q[i * n + i] = 1.0;
        }
        for _ in 0..3 {
            let v: Vec<f64> = (0..n).map(|_| rng.real()).collect();
            let vv: f64 = v.iter().map(|x| x * x).sum();
            if vv == 0.0 {
                continue;
            }
            // q = q (I - 2 v v^T / v^T v)
            for i in 0..n {
                let qv: f64 = (0..n).map(|j| q[i * n + j] * v[j]).sum();
                for j in 0..n {
                    q[i * n + j] -= 2.0 * qv * v[j] / vv;
                }
            }
        }
        let mut a = vec![0.0; n * n];
        for i in 0..n {
            for j in 0..n {
                a[i * n + j] = (0..n).map(|k| q[i * n + k] * eigs[k] * q[j * n + k]).sum();
            }
        }
        a
    }

    fn residual(n: usize, a: &[f64], x: &[f64], b: &[f64]) -> f64 {
        (0..n).map(|i| ((0..n).map(|j| a[i * n + j] * x[j]).sum::<f64>() - b[i]).abs()).fold(0.0, f64::max)
    }

    #[test]
    fn inertia_and_solves_of_random_matrices() {
        let mut rng = Rng(0x2545f4914f6cdd1d);
        for case in 0..300 {
            let n = 1 + case % 12;
            let eigs: Vec<f64> = (0..n)
                .map(|_| {
                    let e = rng.real();
                    let s = 10f64.powf(3.0 * rng.real()); // spread magnitudes
                    if e >= 0.0 { s } else { -s }
                })
                .collect();
            let a = with_eigenvalues(&mut rng, &eigs);
            let f = Ldl::factor(n, &a);
            let pos = eigs.iter().filter(|&&e| e > 0.0).count();
            assert_eq!(f.inertia, Inertia { positive: pos, negative: n - pos, zero: 0 }, "case {case}");
            let b: Vec<f64> = (0..n).map(|_| rng.real()).collect();
            let mut x = b.clone();
            f.solve(&mut x);
            let scale = 1.0 + x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            assert!(residual(n, &a, &x, &b) < 1e-9 * scale * 1e3, "case {case}: residual {}", residual(n, &a, &x, &b));
        }
    }

    #[test]
    fn kkt_matrices_need_two_by_two_pivots() {
        // [[0, 1], [1, 0]] has a zero diagonal: only a 2x2 pivot works.
        let f = Ldl::factor(2, &[0.0, 0.0, 1.0, 0.0]);
        assert_eq!(f.inertia, Inertia { positive: 1, negative: 1, zero: 0 });
        let mut b = vec![3.0, 5.0];
        f.solve(&mut b);
        assert_eq!(b, vec![5.0, 3.0]);
        // A KKT matrix [[H, J^T], [J, 0]] with H = I (2x2), J = [1 1]:
        // inertia (2, 1, 0).
        let k = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0];
        let f = Ldl::factor(3, &k);
        assert_eq!(f.inertia, Inertia { positive: 2, negative: 1, zero: 0 });
        // A rank-deficient J (two equal rows) gives a zero eigenvalue.
        let mut lower = vec![0.0; 16];
        // 4x4: H = I2, J = [[1, 1], [1, 1]].
        let full = [[1.0, 0.0, 1.0, 1.0], [0.0, 1.0, 1.0, 1.0], [1.0, 1.0, 0.0, 0.0], [1.0, 1.0, 0.0, 0.0]];
        for i in 0..4 {
            for j in 0..=i {
                lower[i * 4 + j] = full[i][j];
            }
        }
        let f = Ldl::factor(4, &lower);
        assert_eq!(f.inertia, Inertia { positive: 2, negative: 1, zero: 1 });
    }

    #[test]
    fn singular_and_zero_matrices() {
        let f = Ldl::factor(3, &[0.0; 9]);
        assert_eq!(f.inertia, Inertia { positive: 0, negative: 0, zero: 3 });
        // A numerically singular matrix: the nonzero eigenvalues keep their
        // signs; the zero ones come out as tiny pivots of either sign.
        let mut rng = Rng(7);
        let a = with_eigenvalues(&mut rng, &[2.0, 0.0, -3.0, 0.0, 1.0]);
        let f = Ldl::factor(5, &a);
        let i = f.inertia;
        assert!(i.positive >= 2 && i.negative >= 1 && i.positive + i.negative + i.zero == 5, "{i:?}");
    }
}
