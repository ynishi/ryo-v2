//! Writes a type into `OUT_DIR`, which `src/lib.rs` includes: without build scripts
//! (`OUT_DIR` unset) rust-analyzer cannot expand the `include!` and the type is unknown.

use std::path::Path;

fn main() {
    let out = std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for build scripts");
    let text = "pub struct Generated;\n\nimpl Generated {\n    pub fn new() -> Self {\n        Generated\n    }\n}\n";
    std::fs::write(Path::new(&out).join("generated.rs"), text).expect("write generated.rs");
}
