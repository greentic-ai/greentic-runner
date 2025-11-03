#![allow(dead_code)]

#[cfg(any(feature = "stable-wasmtime", feature = "nightly-wasmtime"))]
pub use wasmtime::component::{Component, Linker};
#[cfg(any(feature = "stable-wasmtime", feature = "nightly-wasmtime"))]
pub use wasmtime::{Engine, Result as WasmResult, Store};
