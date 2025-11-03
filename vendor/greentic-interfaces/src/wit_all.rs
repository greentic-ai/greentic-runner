//! Unified Wasmtime bindings for every WIT world shipped with this crate.
#![allow(clippy::all)]
#![allow(missing_docs)]

macro_rules! declare_world {
    (
        mod $mod_name:ident,
        path = $path_literal:literal,
        world = $world_literal:literal
        $(, legacy = { $($legacy:item)* } )?
    ) => {
        pub mod $mod_name {
            mod bindings {
                wasmtime::component::bindgen!({
                    path: $path_literal,
                    world: $world_literal,
                });
            }

            #[allow(unused_imports)]
            pub use bindings::*;

            $(
                $($legacy)*
            )?
        }
    };
}

#[cfg(feature = "component-v0-4")]
declare_world!(
    mod component_v0_4,
    path = "wit/greentic/component@0.4.0",
    world = "greentic:component/component@0.4.0",
    legacy = {
        use anyhow::Result as AnyResult;
        use wasmtime::component::{Component as WasmtimeComponent, Linker};
        use wasmtime::StoreContextMut;

        pub use bindings::greentic::component::control::Host as ControlHost;

        /// Registers the Greentic control interface with the provided linker.
        pub fn add_control_to_linker<T>(
            linker: &mut Linker<T>,
            get_host: impl Fn(&mut T) -> &mut (dyn ControlHost + Send + Sync + 'static)
                + Send
                + Sync
                + Copy
                + 'static,
        ) -> wasmtime::Result<()>
        where
            T: Send + 'static,
        {
            let mut inst = linker.instance("greentic:component/control@0.4.0")?;

            inst.func_wrap(
                "should-cancel",
                move |mut caller: StoreContextMut<'_, T>, (): ()| {
                    let host = get_host(caller.data_mut());
                    let result = host.should_cancel();
                    Ok((result,))
                },
            )?;

            inst.func_wrap(
                "yield-now",
                move |mut caller: StoreContextMut<'_, T>, (): ()| {
                    let host = get_host(caller.data_mut());
                    host.yield_now();
                    Ok(())
                },
            )?;

            Ok(())
        }

        /// Back-compat shim for instantiating the component.
        pub struct Component;

        impl Component {
            /// Loads the component from raw bytes, mirroring the old helper.
            pub fn instantiate(
                engine: &wasmtime::Engine,
                component_wasm: &[u8],
            ) -> AnyResult<WasmtimeComponent> {
                Ok(WasmtimeComponent::from_binary(engine, component_wasm)?)
            }
        }

        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:component@0.4.0";
    }
);

#[cfg(feature = "pack-export-v0-4")]
declare_world!(
    mod pack_export_v0_4,
    path = "wit/greentic/pack-export@0.4.0",
    world = "greentic:pack-export/pack-exports@0.4.0",
    legacy = {
        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:pack-export@0.4.0";
    }
);

#[cfg(feature = "types-core-v0-4")]
declare_world!(
    mod types_core_v0_4,
    path = "wit/greentic/types-core@0.4.0",
    world = "greentic:types-core/core@0.4.0",
    legacy = {
        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:types-core@0.4.0";
    }
);

#[cfg(feature = "host-import-v0-4")]
declare_world!(
    mod host_import_v0_4,
    path = "wit/greentic/host-import@0.4.0",
    world = "greentic:host-import/host-imports@0.4.0",
    legacy = {
        use wasmtime::component::Linker;
        use wasmtime::{Result, StoreContextMut};

        pub use bindings::greentic::host_import::{http, secrets, telemetry};
        pub use bindings::greentic::types_core::types;

        /// Trait implemented by hosts to service the component imports.
        pub trait HostImports {
            fn secrets_get(
                &mut self,
                key: String,
                ctx: Option<types::TenantCtx>,
            ) -> Result<Result<String, types::IfaceError>>;

            fn telemetry_emit(
                &mut self,
                span_json: String,
                ctx: Option<types::TenantCtx>,
            ) -> Result<()>;

            fn http_fetch(
                &mut self,
                req: http::HttpRequest,
                ctx: Option<types::TenantCtx>,
            ) -> Result<Result<http::HttpResponse, types::IfaceError>>;
        }

        /// Registers the host import functions with the provided linker.
        pub fn add_to_linker<T>(
            linker: &mut Linker<T>,
            get_host: impl Fn(&mut T) -> &mut (dyn HostImports + Send + Sync + 'static)
                + Send
                + Sync
                + Copy
                + 'static,
        ) -> Result<()>
        where
            T: Send + 'static,
        {
            let mut secrets = linker.instance("greentic:host-import/secrets@0.4.0")?;
            secrets.func_wrap(
                "get",
                move |mut caller: StoreContextMut<'_, T>, (key, ctx): (String, Option<types::TenantCtx>)| {
                    let host = get_host(caller.data_mut());
                    host.secrets_get(key, ctx).map(|res| (res,))
                },
            )?;

            let mut telemetry = linker.instance("greentic:host-import/telemetry@0.4.0")?;
            telemetry.func_wrap(
                "emit",
                move |mut caller: StoreContextMut<'_, T>, (span, ctx): (String, Option<types::TenantCtx>)| {
                    let host = get_host(caller.data_mut());
                    host.telemetry_emit(span, ctx)
                },
            )?;

            let mut http_iface = linker.instance("greentic:host-import/http@0.4.0")?;
            http_iface.func_wrap(
                "fetch",
                move |mut caller: StoreContextMut<'_, T>, (req, ctx): (http::HttpRequest, Option<types::TenantCtx>)| {
                    let host = get_host(caller.data_mut());
                    host.http_fetch(req, ctx).map(|res| (res,))
                },
            )?;

            Ok(())
        }

        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:host-import@0.4.0";
    }
);

#[cfg(feature = "host-import-v0-2")]
declare_world!(
    mod host_import_v0_2,
    path = "wit/greentic/host-import@0.2.0",
    world = "greentic:host-import/host-imports@0.2.0",
    legacy = {
        use wasmtime::component::Linker;
        use wasmtime::{Result, StoreContextMut};

        pub use bindings::greentic::host_import::imports;

        /// Trait implemented by hosts to service the component imports.
        pub trait HostImports {
            fn secrets_get(
                &mut self,
                key: String,
                ctx: Option<imports::TenantCtx>,
            ) -> Result<Result<String, imports::IfaceError>>;

            fn telemetry_emit(
                &mut self,
                span_json: String,
                ctx: Option<imports::TenantCtx>,
            ) -> Result<()>;

            fn tool_invoke(
                &mut self,
                tool: String,
                action: String,
                args_json: String,
                ctx: Option<imports::TenantCtx>,
            ) -> Result<Result<String, imports::IfaceError>>;

            fn http_fetch(
                &mut self,
                req: imports::HttpRequest,
                ctx: Option<imports::TenantCtx>,
            ) -> Result<Result<imports::HttpResponse, imports::IfaceError>>;
        }

        /// Registers the host import functions with the provided linker.
        pub fn add_to_linker<T>(
            linker: &mut Linker<T>,
            get_host: impl Fn(&mut T) -> &mut (dyn HostImports + Send + Sync + 'static)
                + Send
                + Sync
                + Copy
                + 'static,
        ) -> Result<()>
        where
            T: Send + 'static,
        {
            let mut inst = linker.instance("greentic:host-import/host-imports@0.2.0")?;

            inst.func_wrap(
                "secrets-get",
                move |mut caller: StoreContextMut<'_, T>,
                      (key, ctx): (String, Option<imports::TenantCtx>)| {
                    let host = get_host(caller.data_mut());
                    host.secrets_get(key, ctx).map(|res| (res,))
                },
            )?;

            inst.func_wrap(
                "telemetry-emit",
                move |mut caller: StoreContextMut<'_, T>,
                      (span, ctx): (String, Option<imports::TenantCtx>)| {
                    let host = get_host(caller.data_mut());
                    host.telemetry_emit(span, ctx)
                },
            )?;

            inst.func_wrap(
                "tool-invoke",
                move |
                    mut caller: StoreContextMut<'_, T>,
                    (tool, action, args, ctx): (String, String, String, Option<imports::TenantCtx>),
                | {
                    let host = get_host(caller.data_mut());
                    host.tool_invoke(tool, action, args, ctx).map(|res| (res,))
                },
            )?;

            inst.func_wrap(
                "http-fetch",
                move |mut caller: StoreContextMut<'_, T>,
                      (req, ctx): (imports::HttpRequest, Option<imports::TenantCtx>)| {
                    let host = get_host(caller.data_mut());
                    host.http_fetch(req, ctx).map(|res| (res,))
                },
            )?;

            Ok(())
        }

        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:host-import@0.2.0";
    }
);

#[cfg(feature = "pack-export-v0-2")]
declare_world!(
    mod pack_export_v0_2,
    path = "wit/greentic/pack-export@0.2.0",
    world = "greentic:pack-export/pack-exports@0.2.0",
    legacy = {
        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:pack-export@0.2.0";
    }
);

#[cfg(feature = "types-core-v0-2")]
declare_world!(
    mod types_core_v0_2,
    path = "wit/greentic/types-core@0.2.0",
    world = "greentic:types-core/core@0.2.0",
    legacy = {
        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:types-core@0.2.0";
    }
);

#[cfg(feature = "secrets-v0-1")]
declare_world!(
    mod secrets_v0_1,
    path = "wit/greentic/secrets@0.1.0",
    world = "greentic:secrets/host@0.1.0",
    legacy = {
        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:secrets@0.1.0";

        /// Raw WIT document for consumers that previously embedded it.
        pub const HOST_WORLD: &str =
            include_str!("../wit/greentic/secrets@0.1.0/package.wit");

        pub fn host_world() -> &'static str {
            HOST_WORLD
        }
    }
);

#[cfg(feature = "oauth-v0-1")]
declare_world!(
    mod oauth_v0_1,
    path = "wit/greentic/oauth@0.1.0",
    world = "greentic:oauth/oauth@0.1.0",
    legacy = {
        /// Canonical package identifier.
        pub const PACKAGE_ID: &str = "greentic:oauth@0.1.0";
    }
);
