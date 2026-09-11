//! Provider-neutral contracts and deterministic safety primitives.

pub mod reasoning;
pub mod transport;
pub mod web;

/// Vendor and process adapters kept outside the provider-neutral core.
pub mod adapters;
pub mod application;
pub mod bootstrap;
pub mod config;
pub mod gitops;
pub mod observability;
pub mod qualification;

/// Pure promotion and gate-admission contracts.
pub mod domain;
/// Runtime policy for loading signed promotion manifests.
pub mod policy;
