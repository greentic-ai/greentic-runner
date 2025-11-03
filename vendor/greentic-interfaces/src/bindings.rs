#![allow(clippy::all)]
#![allow(missing_docs)]
#![allow(unsafe_code)]
#![allow(unused_imports)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Rust bindings generated from the Greentic WIT worlds.
#[cfg(feature = "bindings-rust")]
pub mod generated {
    include!(concat!(
        env!("GREENTIC_INTERFACES_BINDINGS"),
        "/bindings.rs"
    ));
}

#[cfg(feature = "bindings-rust")]
pub use generated::*;
