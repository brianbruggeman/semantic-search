#![no_std]

//! Tier 1 (`no_std` + `alloc`) wire vocabulary shared by `ss` and `ssd`.

extern crate alloc;

mod stats;

pub use stats::{Hit, IndexStats};
