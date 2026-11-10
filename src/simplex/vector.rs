//! Work vectors of the simplex method.

/// What an entry holds after it cancels to zero while it is still listed
/// (HiGHS's `kHighsZero`): small enough to count as zero everywhere, and
/// nonzero, so that the entry isn't listed again when it next changes.
pub const LISTED_ZERO: f64 = 1e-50;

/// A dense vector that lists where its nonzeros are (HiGHS's `HVector`), so
/// that a sparse result is read, updated, and cleared in time proportional
/// to its nonzeros, not its length.
///
/// Every nonzero entry of `array` is in `index` once, unless the vector is
/// `unlisted` (it was filled without listing; any entry may be nonzero). A
/// listed entry may hold zero or `LISTED_ZERO`.
#[derive(Clone, Default)]
pub struct Vector {
    pub array: Vec<f64>,
    pub index: Vec<u32>,
    pub unlisted: bool,
}

/// Reading a vector through its list pays while fewer than one entry in this
/// many is listed; a denser one is scanned whole.
const SCAN_FROM: usize = 4;

impl Vector {
    /// The zero vector of length `len`.
    pub fn new(len: usize) -> Vector {
        Vector { array: vec![0.0; len], index: Vec::new(), unlisted: false }
    }

    /// A vector with these entries, unlisted.
    pub fn unlisted(array: Vec<f64>) -> Vector {
        Vector { array, index: Vec::new(), unlisted: true }
    }

    /// Makes every entry zero.
    pub fn clear(&mut self) {
        if self.sparser_than(2 * SCAN_FROM) {
            for &i in &self.index {
                self.array[i as usize] = 0.0;
            }
        } else {
            self.array.fill(0.0);
        }
        self.index.clear();
        self.unlisted = false;
    }

    /// Adds `v` to entry `i` of a listed vector.
    #[inline]
    pub fn add(&mut self, i: usize, v: f64) {
        let before = self.array[i];
        if before == 0.0 {
            self.index.push(i as u32);
        }
        let after = before + v;
        self.array[i] = if after == 0.0 { LISTED_ZERO } else { after };
    }

    /// Becomes a copy of `other`, which has the same length.
    pub fn copy_from(&mut self, other: &Vector) {
        self.clear();
        if other.sparser_than(SCAN_FROM) {
            for &i in &other.index {
                self.array[i as usize] = other.array[i as usize];
            }
            self.index.extend_from_slice(&other.index);
        } else {
            self.array.copy_from_slice(&other.array);
            self.unlisted = true;
        }
    }

    /// Whether the vector is listed, with fewer than one entry in `one_in`.
    #[inline]
    pub fn sparser_than(&self, one_in: usize) -> bool {
        !self.unlisted && self.index.len() * one_in < self.array.len()
    }

    /// The sum of the entries' squares.
    pub fn norm2(&self) -> f64 {
        let mut sum = 0.0;
        self.for_each(|_, x| sum += x * x);
        sum
    }

    /// Calls `f(index, value)` for every nonzero entry: through the list
    /// if the vector is sparse (in list order), or else by scanning it.
    #[inline(always)]
    pub fn for_each(&self, mut f: impl FnMut(usize, f64)) {
        // One loop for both ways, so that `f` is inlined where it is called.
        let listed = self.sparser_than(SCAN_FROM);
        let count = if listed { self.index.len() } else { self.array.len() };
        for k in 0..count {
            let i = if listed { self.index[k] as usize } else { k };
            let x = self.array[i];
            if x != 0.0 {
                f(i, x);
            }
        }
    }
}

/// A sparse vector as parallel index and value lists.
#[derive(Clone, Default)]
pub struct SparseVec {
    pub index: Vec<u32>,
    pub value: Vec<f64>,
}

impl SparseVec {
    /// Becomes the entries of `v` larger than `tiny`.
    pub fn copy_from(&mut self, v: &Vector, tiny: f64) {
        self.index.clear();
        self.value.clear();
        v.for_each(|i, x| {
            if x.abs() > tiny {
                self.index.push(i as u32);
                self.value.push(x);
            }
        });
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, f64)> + '_ {
        self.index.iter().copied().zip(self.value.iter().copied())
    }
}
