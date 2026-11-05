//! Interval arithmetic for the expression graph's ops: enclosures of the
//! values an op takes when its arguments range over intervals.
//!
//! Every result is widened outward by a few units in the last place, so
//! that rounding in the floating-point operations (and in the library's
//! `exp`, `ln`, `sin`, ...) can't exclude a true value. Functions are
//! restricted to their domains: `ln` of [-2, 4] is `ln` of (0, 4], and of
//! [-2, -1] it is empty: no argument in the interval is valid.

use crate::expr::Func;
use std::f64::consts::{FRAC_PI_2, PI};

const INF: f64 = f64::INFINITY;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Interval {
    pub lo: f64,
    pub hi: f64,
}

/// Widens a bound outward by 4 ulps (relative), keeping infinities. Zero
/// stays zero: widening it by the smallest float would only add bounds like
/// -1e-307, which make relaxations' coefficients unusable, to guard against
/// rounding far below any tolerance.
fn down(v: f64) -> f64 {
    if v.is_finite() { v - 4.0 * f64::EPSILON * v.abs() } else { v }
}

fn up(v: f64) -> f64 {
    if v.is_finite() { v + 4.0 * f64::EPSILON * v.abs() } else { v }
}

/// a * b with 0 * inf = 0 (the convention for interval bounds).
fn mul0(a: f64, b: f64) -> f64 {
    if a == 0.0 || b == 0.0 { 0.0 } else { a * b }
}

impl Interval {
    pub const ENTIRE: Interval = Interval { lo: -INF, hi: INF };
    pub const EMPTY: Interval = Interval { lo: INF, hi: -INF };

    pub fn new(lo: f64, hi: f64) -> Interval {
        Interval { lo, hi }
    }

    pub fn point(v: f64) -> Interval {
        Interval { lo: v, hi: v }
    }

    /// Outward-rounded [lo, hi]; NaN bounds give the entire line.
    fn out(lo: f64, hi: f64) -> Interval {
        if lo.is_nan() || hi.is_nan() {
            return Interval::ENTIRE;
        }
        Interval { lo: down(lo), hi: up(hi) }
    }

    pub fn is_empty(&self) -> bool {
        // Also true for NaN bounds.
        !matches!(self.lo.partial_cmp(&self.hi), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal))
    }

    #[cfg(test)]
    pub fn contains(&self, v: f64) -> bool {
        self.lo <= v && v <= self.hi
    }

    pub fn width(&self) -> f64 {
        self.hi - self.lo
    }

    pub fn intersect(self, o: Interval) -> Interval {
        Interval { lo: self.lo.max(o.lo), hi: self.hi.min(o.hi) }
    }

    pub fn hull(self, o: Interval) -> Interval {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        Interval { lo: self.lo.min(o.lo), hi: self.hi.max(o.hi) }
    }

    pub fn add(self, o: Interval) -> Interval {
        if self.is_empty() || o.is_empty() {
            return Interval::EMPTY;
        }
        Interval::out(self.lo + o.lo, self.hi + o.hi)
    }

    pub fn scale(self, c: f64) -> Interval {
        if self.is_empty() {
            return Interval::EMPTY;
        }
        if c >= 0.0 {
            Interval::out(mul0(c, self.lo), mul0(c, self.hi))
        } else {
            Interval::out(mul0(c, self.hi), mul0(c, self.lo))
        }
    }

    pub fn mul(self, o: Interval) -> Interval {
        if self.is_empty() || o.is_empty() {
            return Interval::EMPTY;
        }
        let p = [mul0(self.lo, o.lo), mul0(self.lo, o.hi), mul0(self.hi, o.lo), mul0(self.hi, o.hi)];
        Interval::out(p.iter().copied().fold(INF, f64::min), p.iter().copied().fold(-INF, f64::max))
    }

    /// The reciprocal; the entire line if the interval contains 0 inside
    /// (one side only, e.g. [0, 2], gives [0.5, inf)).
    pub fn recip(self) -> Interval {
        if self.is_empty() {
            return Interval::EMPTY;
        }
        if self.lo > 0.0 || self.hi < 0.0 {
            Interval::out(1.0 / self.hi, 1.0 / self.lo)
        } else if self.lo == 0.0 && self.hi > 0.0 {
            Interval::out(1.0 / self.hi, INF)
        } else if self.hi == 0.0 && self.lo < 0.0 {
            Interval::out(-INF, 1.0 / self.lo)
        } else if self.lo == 0.0 && self.hi == 0.0 {
            Interval::EMPTY // 1/0 is never defined
        } else {
            Interval::ENTIRE
        }
    }

    pub fn div(self, o: Interval) -> Interval {
        self.mul(o.recip())
    }

    pub fn powi(self, n: i32) -> Interval {
        if self.is_empty() {
            return Interval::EMPTY;
        }
        if n == 0 {
            return Interval::point(1.0);
        }
        if n < 0 {
            return self.powi(-n).recip();
        }
        let (a, b) = (self.lo.powi(n), self.hi.powi(n));
        if n % 2 == 1 || self.lo >= 0.0 {
            Interval::out(a, b)
        } else if self.hi <= 0.0 {
            Interval::out(b, a)
        } else {
            Interval::out(0.0, a.max(b))
        }
    }

    /// x^c for a non-integer constant c (defined for x >= 0; x > 0 if c < 0).
    pub fn powc(self, c: f64) -> Interval {
        let d = self.intersect(Interval::new(0.0, INF));
        if d.is_empty() {
            return Interval::EMPTY;
        }
        let (a, b) = (d.lo.powf(c), d.hi.powf(c));
        if c > 0.0 { Interval::out(a, b) } else { Interval::out(b, a) }
    }

    /// a^b = exp(b ln a), for a > 0.
    pub fn pow(self, b: Interval) -> Interval {
        b.mul(self.func(Func::Ln)).func(Func::Exp)
    }

    pub fn func(self, f: Func) -> Interval {
        if self.is_empty() {
            return Interval::EMPTY;
        }
        let (lo, hi) = (self.lo, self.hi);
        match f {
            Func::Exp => Interval::out(lo.exp(), hi.exp()).intersect(Interval::new(0.0, INF)),
            Func::Ln => {
                if hi <= 0.0 {
                    return Interval::EMPTY;
                }
                Interval::out(if lo <= 0.0 { -INF } else { lo.ln() }, hi.ln())
            }
            Func::Sqrt => {
                if hi < 0.0 {
                    return Interval::EMPTY;
                }
                Interval::out(lo.max(0.0).sqrt(), hi.sqrt()).intersect(Interval::new(0.0, INF))
            }
            Func::Atan => Interval::out(lo.atan(), hi.atan()).intersect(Interval::new(-FRAC_PI_2, FRAC_PI_2)),
            Func::Tanh => Interval::out(lo.tanh(), hi.tanh()).intersect(Interval::new(-1.0, 1.0)),
            Func::Sin => periodic(lo, hi, f64::sin, FRAC_PI_2),
            Func::Cos => periodic(lo, hi, f64::cos, 0.0),
            Func::Tan => {
                if !(lo.is_finite() && hi.is_finite()) || hi - lo >= PI {
                    return Interval::ENTIRE;
                }
                // A pole pi/2 + k pi in [lo, hi] makes it unbounded.
                let k = ((lo - FRAC_PI_2) / PI).ceil();
                if FRAC_PI_2 + k * PI <= hi { Interval::ENTIRE } else { Interval::out(lo.tan(), hi.tan()) }
            }
        }
    }
}

/// The range of sin or cos on [lo, hi]: the endpoint values, and 1 or -1
/// where a maximum (at `peak + 2k pi`) or minimum (`peak + pi + 2k pi`) lies
/// inside.
fn periodic(lo: f64, hi: f64, f: fn(f64) -> f64, peak: f64) -> Interval {
    if !(lo.is_finite() && hi.is_finite()) || hi - lo >= 2.0 * PI {
        return Interval::new(-1.0, 1.0);
    }
    let contains = |at: f64| {
        let k = ((lo - at) / (2.0 * PI)).ceil();
        at + k * 2.0 * PI <= hi
    };
    let (a, b) = (f(lo), f(hi));
    let top = if contains(peak) { 1.0 } else { a.max(b) };
    let bottom = if contains(peak + PI) { -1.0 } else { a.min(b) };
    Interval::out(bottom, top).intersect(Interval::new(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn real(&mut self, lo: f64, hi: f64) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            lo + (hi - lo) * (self.0 % 1_000_001) as f64 / 1e6
        }
    }

    /// Every op's enclosure contains the op's value at sampled points
    /// (where it is defined).
    #[test]
    fn enclosures_contain_sampled_values() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for _ in 0..3000 {
            let a = {
                let (p, q) = (rng.real(-6.0, 6.0), rng.real(-6.0, 6.0));
                Interval::new(p.min(q), p.max(q))
            };
            let b = {
                let (p, q) = (rng.real(-3.0, 3.0), rng.real(-3.0, 3.0));
                Interval::new(p.min(q), p.max(q))
            };
            let c = rng.real(-2.5, 2.5);
            let n = (rng.real(-4.0, 4.0).round()) as i32;
            for _ in 0..20 {
                let (x, y) = (rng.real(a.lo, a.hi), rng.real(b.lo, b.hi));
                let check = |what: &str, i: Interval, v: f64| {
                    if v.is_finite() {
                        assert!(i.contains(v), "{what}: {v} not in {i:?} (a = {a:?}, b = {b:?}, x = {x}, y = {y})");
                    }
                };
                check("add", a.add(b), x + y);
                check("scale", a.scale(c), c * x);
                check("mul", a.mul(b), x * y);
                check("div", a.div(b), x / y);
                check("powi", a.powi(n), x.powi(n));
                check("powc", a.powc(c), x.powf(c));
                check("pow", a.pow(b), x.powf(y));
                for f in Func::ALL {
                    check(f.name(), a.func(f), f.apply(x));
                }
            }
        }
    }

    #[test]
    fn domains_and_special_cases() {
        assert!(Interval::new(-2.0, -1.0).func(Func::Ln).is_empty());
        assert!(Interval::new(-2.0, -1.0).func(Func::Sqrt).is_empty());
        assert_eq!(Interval::new(-2.0, 4.0).func(Func::Ln).lo, -INF);
        assert!(Interval::new(-1.0, 2.0).powi(2).lo <= 0.0 && Interval::new(-1.0, 2.0).powi(2).hi >= 4.0);
        assert_eq!(Interval::new(-1.0, 1.0).recip(), Interval::ENTIRE);
        assert!(Interval::new(0.0, 0.0).recip().is_empty());
        let s = Interval::new(0.0, PI).func(Func::Sin);
        assert!(s.hi >= 1.0 && s.lo <= 0.0 && s.lo > -1e-10);
        assert_eq!(Interval::new(1.0, 2.0).func(Func::Tan), Interval::ENTIRE); // pi/2 inside
        // Rounding is outward: the enclosure of an exact point contains it.
        let e = Interval::point(1.0).func(Func::Exp);
        assert!(e.lo < 1f64.exp() && e.hi > 1f64.exp());
    }
}
