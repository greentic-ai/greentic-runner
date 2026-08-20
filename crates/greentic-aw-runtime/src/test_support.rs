//! Test-only constructors shared by this crate's unit tests, its integration
//! tests, and the `aw-serve` canned-reply harness.

/// Build an [`ExtensionRuntime`](greentic_ext_runtime::ExtensionRuntime) for
/// tests.
///
/// This lane's greentic-ext-runtime exposes no `for_test()` constructor, so
/// build the equivalent by hand: a runtime rooted at a discovery path that
/// holds no extensions, carrying the crate's own test host overrides. Every
/// lookup therefore yields an empty tool catalog, which is what these tests
/// want — they drive canned LLM replies and never dispatch to a real
/// extension.
///
/// # Panics
/// If wasmtime cannot initialise its engine. Unrecoverable in a test process,
/// and every caller is a test constructor with nowhere to return an error to.
#[must_use]
#[allow(clippy::expect_used)]
pub fn extension_runtime() -> greentic_ext_runtime::ExtensionRuntime {
    greentic_ext_runtime::ExtensionRuntime::new(greentic_ext_runtime::RuntimeConfig::from_paths(
        greentic_ext_runtime::DiscoveryPaths::new(std::path::PathBuf::from(
            "/nonexistent/greentic-aw-runtime-test-extensions",
        )),
    ))
    .expect("wasmtime engine init for the test extension runtime")
    .with_host_overrides(greentic_ext_runtime::HostOverrides::defaults_for_tests())
}
