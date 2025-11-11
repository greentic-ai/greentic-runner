use anyhow::Result;
use wasmtime_wasi::p2;

use crate::runtime_wasmtime::Linker;

use crate::pack::ComponentState;

pub fn register_all(linker: &mut Linker<ComponentState>) -> Result<()> {
    p2::add_to_linker_sync(linker)?;
    greentic_interfaces::host_import_v0_6::add_to_linker(linker, |state| state)?;
    greentic_interfaces::host_import_v0_2::add_to_linker(linker, |state| state)
}
