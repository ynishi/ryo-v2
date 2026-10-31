//! A crate whose type comes from its build script (`build.rs`).

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

pub fn make() -> Generated {
    Generated::new()
}
