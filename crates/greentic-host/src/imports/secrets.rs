use anyhow::Result;
use wasmtime::component::{Caller, Linker};
use wasmtime::Error;

use crate::pack::HostState;

pub fn register(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap(
        "greentic:host/secrets",
        "get",
        |caller: Caller<'_, HostState>,
         key: String|
         -> std::result::Result<Option<String>, Error> {
            let state = caller.data();
            match state.get_secret(&key) {
                Ok(value) => Ok(Some(value)),
                Err(err) => {
                    tracing::warn!(secret = %key, error = %err, "secret lookup denied");
                    Ok(None)
                }
            }
        },
    )?;
    Ok(())
}
