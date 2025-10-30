pub mod http;
pub mod secrets;
pub mod telemetry;

use anyhow::Result;
use wasmtime::component::Linker;

use crate::pack::HostState;

pub fn register_all(linker: &mut Linker<HostState>) -> Result<()> {
    secrets::register(linker)?;
    telemetry::register(linker)?;
    http::register(linker)?;
    Ok(())
}
