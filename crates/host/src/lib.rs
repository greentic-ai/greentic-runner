#![forbid(unsafe_code)]

pub mod config;
pub mod imports;

#[cfg(feature = "new-runner")]
pub mod glue;

#[cfg(feature = "new-runner")]
pub mod newrunner;

#[cfg(any(feature = "stable-wasmtime", feature = "nightly-wasmtime"))]
pub mod pack;

#[cfg(any(feature = "stable-wasmtime", feature = "nightly-wasmtime"))]
pub mod runner;

#[cfg(any(feature = "stable-wasmtime", feature = "nightly-wasmtime"))]
pub mod runtime_wasmtime;

#[cfg(any(feature = "stable-wasmtime", feature = "nightly-wasmtime"))]
pub mod desktop;

pub mod telemetry;
pub mod verify;
