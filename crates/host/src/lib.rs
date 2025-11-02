#![forbid(unsafe_code)]

#[cfg(feature = "new-runner")]
pub mod glue;

#[cfg(feature = "new-runner")]
pub mod newrunner;

#[cfg(any(feature = "stable-wasmtime", feature = "nightly-wasmtime"))]
pub mod runtime_wasmtime;
