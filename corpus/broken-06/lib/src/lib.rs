//! CrossCrateExtractTrait fixture — lib side (P1 baseline).
//!
//! Each case is an RL061 ExtractTrait candidate shape (inherent impl,
//! >= 5 instance methods, no pattern args / body self-mut / associated
//! fn / const fn — except where an axis injects one deliberately). The
//! user-side crate supplies cross-crate callers so the sweep probe can
//! observe the current R15 wholesale-skip behaviour per caller shape:
//!
//!   Case01 — cross-crate dot callers only        (R15_CROSS_DOT)
//!   Case02 — cross-crate type-path callers only  (R15_CROSS_TYPEPATH)
//!   Case03 — mixed same-crate + cross-crate      (R15_MIXED)
//!   Case04 — same-crate callers only             (BASE_SAMECRATE, R15 must NOT fire)
//!   Case05 — associated fn + cross-crate caller  (R12_R15_MULTIAXIS)
//!
//! P1 baseline expectation (R15 active, CrossCrateExtractTrait not yet
//! implemented): Case01/02/03/05 = (b) correct reject, Case04 = PASS.
//! The P3 suggester relaxation flips Case01/02 (and possibly 03) to
//! PASS via the CrossCrateExtractTrait path.

// Case01: minimal candidate, cross-crate dot callers only.
pub struct Case01 {
    pub base: u32,
}
impl Case01 {
    pub fn one(&self) -> u32 {
        self.base + 1
    }
    pub fn two(&self) -> u32 {
        self.base + 2
    }
    pub fn three(&self) -> u32 {
        self.base + 3
    }
    pub fn four(&self) -> u32 {
        self.base + 4
    }
    pub fn five(&self) -> u32 {
        self.base + 5
    }
}

// Case02: cross-crate type-path (inherent UFCS) callers only.
pub struct Case02 {
    pub width: u32,
    pub height: u32,
}
impl Case02 {
    pub fn area(&self) -> u32 {
        self.width * self.height
    }
    pub fn perimeter(&self) -> u32 {
        2 * (self.width + self.height)
    }
    pub fn is_square(&self) -> bool {
        self.width == self.height
    }
    pub fn longer_side(&self) -> u32 {
        if self.width > self.height {
            self.width
        } else {
            self.height
        }
    }
    pub fn aspect_sum(&self) -> u32 {
        self.width + self.height
    }
}

// Case03: mixed — same-crate caller below + cross-crate caller in user.
pub struct Case03(pub i64);
impl Case03 {
    pub fn value(&self) -> i64 {
        self.0
    }
    pub fn doubled(&self) -> i64 {
        self.0 * 2
    }
    pub fn negated(&self) -> i64 {
        -self.0
    }
    pub fn is_positive(&self) -> bool {
        self.0 > 0
    }
    pub fn magnitude(&self) -> i64 {
        self.0.abs()
    }
}
// Same-crate caller — pairs with the cross-crate caller in user to
// make Case03 the MIXED axis.
pub fn use_case03_same_crate(c: &Case03) -> i64 {
    c.value() + c.doubled()
}

// Case04: same-crate callers only — the R15 scanner must NOT fire, so
// the existing SingleCrate ExtractTrait path stays live inside a
// multi-crate workspace (baseline evidence that Api::from_path over a
// 2-crate workspace keeps RL061 end-to-end intact).
pub struct Case04 {
    pub name_len: usize,
}
impl Case04 {
    pub fn len(&self) -> usize {
        self.name_len
    }
    pub fn is_empty(&self) -> bool {
        self.name_len == 0
    }
    pub fn padded(&self) -> usize {
        self.name_len + 8
    }
    pub fn halved(&self) -> usize {
        self.name_len / 2
    }
    pub fn squared(&self) -> usize {
        self.name_len * self.name_len
    }
}
pub fn use_case04_same_crate(c: &Case04) -> usize {
    c.len() + c.padded()
}

// Case05: associated fn (constructor) + cross-crate caller — the R12
// axis fires before R15 is even consulted; the probe pins the
// multi-axis reject so the P3 relaxation cannot accidentally unlock
// impls that R12 must keep rejecting.
pub struct Case05 {
    pub seed: u32,
    pub scale: u32,
}
impl Case05 {
    pub fn new(seed: u32) -> Self {
        Case05 { seed }
    }
    pub fn next(&self) -> u32 {
        self.seed + 1
    }
    pub fn prev(&self) -> u32 {
        self.seed.saturating_sub(1)
    }
    pub fn twice(&self) -> u32 {
        self.seed * 2
    }
    pub fn half(&self) -> u32 {
        self.seed / 2
    }
    pub fn is_even(&self) -> bool {
        self.seed % 2 == 0
    }
}

// Case06: consumed ONLY through the facade crate's re-export by the
// viewer crate (which has no direct dependency on this lib crate).
// The P3 direct-dep guard must wholesale-skip this impl: the executor
// cannot inject a resolvable trait `use` into viewer.
pub struct Case06 {
    pub seed: u32,
}
impl Case06 {
    pub fn first(&self) -> u32 {
        self.seed + 1
    }
    pub fn second(&self) -> u32 {
        self.seed
    }
    pub fn third(&self) -> u32 {
        self.seed * 2
    }
    pub fn fourth(&self) -> u32 {
        self.seed * 3
    }
    pub fn fifth(&self) -> u32 {
        self.seed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case01_methods() {
        let c = Case01 { base: 10 };
        assert_eq!(c.one() + c.two() + c.three() + c.four() + c.five(), 65);
    }

    #[test]
    fn case02_methods() {
        let c = Case02 {
            width: 3,
            height: 4,
        };
        assert_eq!(c.area(), 12);
        assert_eq!(c.perimeter(), 14);
        assert!(!c.is_square());
        assert_eq!(c.longer_side(), 4);
        assert_eq!(c.aspect_sum(), 7);
    }

    #[test]
    fn case03_same_crate_caller() {
        let c = Case03(-5);
        assert_eq!(use_case03_same_crate(&c), -15);
        assert_eq!(c.negated(), 5);
        assert!(!c.is_positive());
        assert_eq!(c.magnitude(), 5);
    }

    #[test]
    fn case04_same_crate_caller() {
        let c = Case04 { name_len: 4 };
        assert_eq!(use_case04_same_crate(&c), 16);
        assert!(!c.is_empty());
        assert_eq!(c.halved(), 2);
        assert_eq!(c.squared(), 16);
    }

    #[test]
    fn case05_methods() {
        let c = Case05::new(4);
        assert_eq!(c.next(), 5);
        assert_eq!(c.prev(), 3);
        assert_eq!(c.twice(), 8);
        assert_eq!(c.half(), 2);
        assert!(c.is_even());
    }
}
