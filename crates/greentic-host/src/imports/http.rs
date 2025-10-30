use anyhow::Result;
use wasmtime::component::Linker;

use crate::pack::HostState;

pub fn register(linker: &mut Linker<HostState>) -> Result<()> {
    let _ = linker;
    // TODO: expose http.fetch binding when enabled
    Ok(())
}
