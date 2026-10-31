//! CrossCrateExtractTrait fixture — viewer side (P3 guard axis).
//!
//! Calls `Case06` methods through the facade re-export, with NO direct
//! dependency on the lib crate. This is the unsound residual for the
//! P3 relaxation: the executor's cross-crate use inject would emit
//! `use extract_trait_multicrate_lib::Case06Trait;` here, which cannot
//! resolve (lib is absent from this crate's `[dependencies]`). The
//! suggester's direct-dep guard must therefore wholesale-skip Case06.

use extract_trait_multicrate_facade::Case06;

pub fn dot_case06(c: &Case06) -> u32 {
    c.first() + c.second()
}
pub fn dot_case06_more(c: &Case06) -> u32 {
    c.third() + c.fourth() + c.fifth()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case06_via_facade() {
        let c = Case06 { seed: 3 };
        assert_eq!(dot_case06(&c), 7);
        assert_eq!(dot_case06_more(&c), 18);
    }
}
