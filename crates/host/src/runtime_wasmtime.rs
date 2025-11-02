#![allow(dead_code)]

#[cfg(all(feature = "stable-wasmtime", feature = "nightly-wasmtime"))]
compile_error!("Enable only one of stable-wasmtime or nightly-wasmtime.");

#[cfg(feature = "stable-wasmtime")]
mod stable {
    pub use wasmtime_stable::component::{Component, Linker};
    pub use wasmtime_stable::{Engine, Result as WasmResult, Store};
}

#[cfg(feature = "nightly-wasmtime")]
mod nightly {
    pub use wasmtime_nightly::component::{Component, Linker};
    pub use wasmtime_nightly::{Engine, Result as WasmResult, Store};
}

#[cfg(feature = "stable-wasmtime")]
pub use stable::*;

#[cfg(all(not(feature = "stable-wasmtime"), feature = "nightly-wasmtime"))]
pub use nightly::*;
