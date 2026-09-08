//! U7 integration-test harness for Tempo export degradation contracts.

#[path = "integration/otlp_degradation.rs"]
mod otlp_degradation;

#[path = "integration/incident_trace_failure.rs"]
mod incident_trace_failure;
