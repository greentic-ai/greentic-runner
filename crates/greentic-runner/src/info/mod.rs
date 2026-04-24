//! Build metadata and runtime capability surface for `greentic-runner info`.
//!
//! - [`report`] defines the [`InfoReport`] struct and the `collect()` function
//!   that assembles it from compile-time env vars and hand-maintained interface
//!   lists.
//! - `human` (added in a follow-up commit) renders an [`InfoReport`] as a
//!   concise human-readable string.

pub mod report;

#[allow(unused_imports)]
pub use report::{InfoReport, collect};
