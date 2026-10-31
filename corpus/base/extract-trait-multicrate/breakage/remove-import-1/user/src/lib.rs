//! CrossCrateExtractTrait fixture — user side (P1 baseline).
//!
//! Supplies the cross-crate callers that make the lib-side inherent
//! impls fire (or not fire) the R15 wholesale-skip scanner:
//!
//!   dot_case01   — `instance.method()` dot calls        (Case01)
//!   path_case02  — `Case02::method(&s)` type-path calls (Case02)
//!   dot_case03   — dot call pairing with lib's same-crate caller (MIXED)
//!   (Case04)     — deliberately NOT referenced here (same-crate only axis)
//!   dot_case05   — dot call on an impl that also carries an associated fn
//!
//! Naming follows the inline-trait-crosscrate convention so the sweep
//! probe can attribute coverage by case label.


// ===== Case01 — cross-crate dot callers =====

pub fn dot_case01(c: &Case01) -> u32 {
    c.one() + c.two() + c.three()
}
pub fn dot_case01_more(c: &Case01) -> u32 {
    c.four() + c.five()
}

// ===== Case02 — cross-crate type-path (inherent UFCS) callers =====

pub fn path_case02(c: &Case02) -> u32 {
    Case02::area(c) + Case02::perimeter(c)
}
pub fn path_case02_more(c: &Case02) -> u32 {
    let side = Case02::longer_side(c);
    if Case02::is_square(c) {
        side
    } else {
        side + Case02::aspect_sum(c)
    }
}

// ===== Case03 — cross-crate side of the MIXED axis =====

pub fn dot_case03(c: &Case03) -> i64 {
    c.negated() + c.magnitude()
}

// ===== Case05 — cross-crate caller on an R12-carrying impl =====

pub fn dot_case05(c: &Case05) -> u32 {
    c.next() + c.twice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case01_cross_crate() {
        let c = Case01 { base: 10 };
        assert_eq!(dot_case01(&c), 36);
        assert_eq!(dot_case01_more(&c), 29);
    }

    #[test]
    fn case02_cross_crate() {
        let c = Case02 {
            width: 3,
            height: 4,
        };
        assert_eq!(path_case02(&c), 26);
        assert_eq!(path_case02_more(&c), 11);
    }

    #[test]
    fn case03_cross_crate() {
        let c = Case03(-5);
        assert_eq!(dot_case03(&c), 10);
    }

    #[test]
    fn case05_cross_crate() {
        let c = Case05 { seed: 4 };
        assert_eq!(dot_case05(&c), 13);
    }
}
