//! External-system adapters implementing capability-owned boundary contracts.

/// Disposable typed target-gateway protocol.
pub mod gateway;

/// Shadow-report notification adapter.
pub mod ntfy;

/// Read-only Grafana adapters.
pub mod grafana;

/// Language-model provider adapters.
pub mod llm;
