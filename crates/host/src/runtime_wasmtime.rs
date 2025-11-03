#![allow(dead_code)]

#[cfg(all(feature = "stable-wasmtime", not(feature = "nightly-wasmtime")))]
mod stable {
    pub use wasmtime_stable::component::{Component, Linker};
    pub use wasmtime_stable::{Engine, Result as WasmResult, Store};
}

#[cfg(feature = "nightly-wasmtime")]
mod nightly {
    pub use wasmtime_nightly::component::{Component, Linker};
    pub use wasmtime_nightly::{Engine, Result as WasmResult, Store};
}

#[cfg(feature = "nightly-wasmtime")]
pub use nightly::*;

#[cfg(all(feature = "stable-wasmtime", not(feature = "nightly-wasmtime")))]
pub use stable::*;
