//! Sealing is a type: the misuses of `tests/compile-fail/` do not compile. Their expected
//! `.stderr` files are compiler output of the toolchain `rust-toolchain.toml` pins; a toolchain
//! bump regenerates them in the same change (`TRYBUILD=overwrite`, reviewed as a diff).

#[test]
fn sealing_types_refuse_misuse() {
    trybuild::TestCases::new().compile_fail("tests/compile-fail/*.rs");
}
