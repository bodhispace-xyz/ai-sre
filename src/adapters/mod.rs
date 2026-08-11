//! External-system adapters implementing capability-owned boundary contracts.

/// Disposable typed target-gateway protocol.
pub mod gateway;

/// Server-owned read-only Git, deployment, and health context adapters.
pub mod context;

/// Shadow-report notification adapter.
pub mod ntfy;

/// Read-only Grafana adapters.
pub mod grafana;

/// Language-model provider adapters.
pub mod llm;
