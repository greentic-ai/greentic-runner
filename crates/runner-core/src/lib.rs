//! Pack runtime core for Greentic runner.
//!
//! This crate provides the building blocks required to ingest pack indexes,
//! download pack artifacts from multiple backends, verify their integrity, and
//! maintain an on-disk cache that other runtimes can consume.

pub mod env;
pub mod packs;

pub use env::{IndexLocation, PackConfig, PackSource};
pub use packs::{
    Index, PackDigest, PackManager, PackRef, PackVersion, ResolvedPack, ResolvedSet, TenantPacks,
};
