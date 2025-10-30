use anyhow::Result;
use wasmtime::component::Linker;

use crate::pack::HostState;

pub fn register(linker: &mut Linker<HostState>) -> Result<()> {
    let _ = linker;
    // TODO: wire telemetry.emit to tracing
    Ok(())
}
