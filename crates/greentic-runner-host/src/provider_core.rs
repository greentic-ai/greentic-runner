#![allow(clippy::allow_attributes)]

// Legacy provider-core bindings for greentic:provider-core@1.0.0.
pub mod schema_core {
    wasmtime::component::bindgen!({
        inline: r#"
        package greentic:provider-core@1.0.0;

        /// Core provider runtime surface.
        interface schema-core-api {
          /// Result of validating a provider configuration payload as JSON bytes.
          type validation-result = list<u8>;
          /// Health status reported by the provider runtime as JSON bytes.
          type health-status = list<u8>;
          /// Result of invoking a provider operation as JSON bytes.
          type invoke-result = list<u8>;

          /// Return the provider manifest as JSON bytes.
          describe: func() -> validation-result;
          /// Validate a provider configuration JSON payload.
          validate-config: func(config-json: list<u8>) -> validation-result;
          /// Lightweight health probe.
          healthcheck: func() -> health-status;
          /// Invoke an operation with JSON input.
          invoke: func(op: string, input-json: list<u8>) -> invoke-result;
        }

        world schema-core {
          export schema-core-api;
        }
        "#,
        world: "schema-core",
    });
}

// Provider schema-core bindings for greentic:provider-schema-core@1.0.0.
pub mod schema_core_schema {
    wasmtime::component::bindgen!({
        inline: r#"
        package greentic:provider-schema-core@1.0.0;

        /// Core provider runtime surface.
        interface schema-core-api {
          /// Result of validating a provider configuration payload as JSON bytes.
          type validation-result = list<u8>;
          /// Health status reported by the provider runtime as JSON bytes.
          type health-status = list<u8>;
          /// Result of invoking a provider operation as JSON bytes.
          type invoke-result = list<u8>;

          /// Return the provider manifest as JSON bytes.
          describe: func() -> validation-result;
          /// Validate a provider configuration JSON payload.
          validate-config: func(config-json: list<u8>) -> validation-result;
          /// Lightweight health probe.
          healthcheck: func() -> health-status;
          /// Invoke an operation with JSON input.
          invoke: func(op: string, input-json: list<u8>) -> invoke-result;
        }

        world schema-core {
          export schema-core-api;
        }
        "#,
        world: "schema-core",
    });
}

// Provider schema-core bindings for greentic:provider@1.0.0 (legacy path namespace).
pub mod schema_core_path {
    wasmtime::component::bindgen!({
        inline: r#"
        package greentic:provider@1.0.0;

        /// Core provider runtime surface.
        interface schema-core-api {
          /// Result of validating a provider configuration payload as JSON bytes.
          type validation-result = list<u8>;
          /// Health status reported by the provider runtime as JSON bytes.
          type health-status = list<u8>;
          /// Result of invoking a provider operation as JSON bytes.
          type invoke-result = list<u8>;

          /// Return the provider manifest as JSON bytes.
          describe: func() -> validation-result;
          /// Validate a provider configuration JSON payload.
          validate-config: func(config-json: list<u8>) -> validation-result;
          /// Lightweight health probe.
          healthcheck: func() -> health-status;
          /// Invoke an operation with JSON input.
          invoke: func(op: string, input-json: list<u8>) -> invoke-result;
        }

        world schema-core {
          export schema-core-api;
        }
        "#,
        world: "schema-core",
    });
}

pub use schema_core::SchemaCore as LegacySchemaCore;
pub use schema_core::SchemaCorePre as LegacySchemaCorePre;
pub use schema_core_path::SchemaCore as PathSchemaCore;
pub use schema_core_path::SchemaCorePre as PathSchemaCorePre;
pub use schema_core_schema::SchemaCore as SchemaSchemaCore;
pub use schema_core_schema::SchemaCorePre as SchemaSchemaCorePre;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_provider_core_exports_are_distinct_types() {
        let legacy = std::any::type_name::<LegacySchemaCorePre<()>>();
        let path = std::any::type_name::<PathSchemaCorePre<()>>();
        let schema = std::any::type_name::<SchemaSchemaCorePre<()>>();

        assert!(legacy.contains("SchemaCorePre"));
        assert!(path.contains("SchemaCorePre"));
        assert!(schema.contains("SchemaCorePre"));
        assert_ne!(legacy, path);
        assert_ne!(legacy, schema);
    }
}
