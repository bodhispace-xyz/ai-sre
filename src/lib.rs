//! Provider-neutral contracts and deterministic safety primitives.

pub mod reasoning;
pub mod transport;

/// Vendor and process adapters kept outside the provider-neutral core.
pub mod adapters;
pub mod bootstrap;
pub mod config;
