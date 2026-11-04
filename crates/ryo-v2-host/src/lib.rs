//! The modules the host's binaries share: `ryo-v2` (its `bench` command) and `engine-probe` run
//! the one stacked procedure of [`stacked`] and read this process's figures through [`proc`], so
//! that what one measures is what the other times.
//!
//! The host runs on Linux only: [`proc`] reads `/proc`, and a read that fails is an error of the
//! run.

pub mod bench;
pub mod engine_probe;
pub mod layer_cost;
pub mod proc;
pub mod stacked;
