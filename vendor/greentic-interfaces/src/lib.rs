#![deny(unsafe_code)]
#![warn(missing_docs, clippy::unwrap_used, clippy::expect_used)]
#![doc = include_str!("../README.md")]

//! ABI-oriented bindings for Greentic WIT packages.

pub mod bindings;
pub mod wit_all;
pub use wit_all::*;
pub mod mappers;
pub mod validate;
