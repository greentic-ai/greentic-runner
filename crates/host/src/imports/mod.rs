use anyhow::Result;
use wasmtime::component::Linker;

use crate::pack::HostState;

pub fn register_all(linker: &mut Linker<HostState>) -> Result<()> {
    greentic_interfaces::host_import_v0_2::add_to_linker(linker, |state| state)
}
