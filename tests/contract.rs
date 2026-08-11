//! Executable contracts for provider, Grafana, and gateway boundaries.

#[path = "contract/providers.rs"]
mod providers;

#[path = "contract/gcx.rs"]
mod gcx;

#[path = "contract/openai.rs"]
mod openai;

#[path = "contract/gateway.rs"]
mod gateway;

#[path = "contract/incident.rs"]
mod incident;

#[path = "contract/investigation.rs"]
mod investigation;

#[path = "contract/ntfy.rs"]
mod ntfy;

#[path = "contract/transport.rs"]
mod transport;

#[path = "contract/config.rs"]
mod config;

#[path = "contract/dispatcher.rs"]
mod dispatcher;

#[path = "contract/live.rs"]
mod live;
