//! Pack module - loading, executing, and managing packs.
//!
//! This module contains:
//! - [`PackRuntime`] - Main entry point for loading and executing packs
//! - [`PackMetadata`] - Metadata about loaded packs
//! - [`FlowDescriptor`] - Description of flows within a pack
//! - [`I18nCatalog`] - Internationalization catalog for translations
//! - [`ComponentResolution`] - Configuration for component resolution
//! - [`HostState`] - Host state for WASM component execution
//! - [`ComponentState`] - Component state for WASM instances

mod component_state;
mod flows;
mod helpers;
mod host_state;
mod host_traits;
mod i18n;
mod loaders;
mod metadata;
mod resolution;
mod runtime;

// Re-export public types
pub use component_state::{ComponentState, add_component_control_to_linker, register_all};
pub use flows::FlowDescriptor;
pub use host_state::HostState;
pub use i18n::I18nCatalog;
pub use metadata::PackMetadata;
pub use resolution::ComponentResolution;
pub use runtime::PackRuntime;
