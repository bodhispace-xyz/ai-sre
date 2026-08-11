//! Alert normalization and in-process incident correlation.
//!
//! This module converts Alertmanager-shaped signals into a stable incident
//! identity before evidence collection or provider calls begin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An incoming alert or recovery notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertSignal {
    /// Alert status, normally `firing` or `resolved`.
    pub status: AlertStatus,
    /// Stable Alertmanager fingerprint when one is available.
    #[serde(default)]
    pub fingerprint: String,
    /// Labels identifying the alert and affected resource.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Human-readable alert context.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

/// Alertmanager webhook envelope accepted by the HTTP intake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertmanagerWebhook {
    /// Alertmanager payload schema version.
    #[serde(default)]
    pub version: String,
    /// Group identity assigned by Alertmanager.
    #[serde(rename = "groupKey", default)]
    pub group_key: String,
    /// Number of alerts omitted by Alertmanager truncation.
    #[serde(rename = "truncatedAlerts", default)]
    pub truncated_alerts: u32,
    /// Aggregate group status.
    #[serde(default)]
    pub status: String,
    /// Configured receiver name.
    #[serde(default)]
    pub receiver: String,
    /// Common labels for the alert group.
    #[serde(rename = "groupLabels", default)]
    pub group_labels: BTreeMap<String, String>,
    /// Labels shared by all alerts in the group.
    #[serde(rename = "commonLabels", default)]
    pub common_labels: BTreeMap<String, String>,
    /// Annotations shared by all alerts in the group.
    #[serde(rename = "commonAnnotations", default)]
    pub common_annotations: BTreeMap<String, String>,
    /// Alertmanager external URL.
    #[serde(rename = "externalURL", default)]
    pub external_url: String,
    /// Alerts delivered in this webhook batch.
    #[serde(default)]
    pub alerts: Vec<AlertSignal>,
}

/// Normalizes every alert in one Alertmanager delivery.
pub fn normalize_webhook(webhook: AlertmanagerWebhook) -> Vec<IncidentSignal> {
    webhook.alerts.into_iter().map(normalize).collect()
}

/// Lifecycle state carried by an incoming alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertStatus {
    /// The condition is currently active.
    Firing,
    /// The condition has recovered.
    Resolved,
}

/// Normalized signal handed to the incident workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentSignal {
    /// Alertmanager correlation key shared by all lifecycle episodes.
    pub correlation_id: String,
    /// Stable identity shared by duplicate firing and recovery events.
    pub incident_id: String,
    /// Monotonic lifecycle episode number for this correlation key.
    pub episode: u64,
    /// Normalized alert name.
    pub alert_name: String,
    /// Lifecycle state of the signal.
    pub status: AlertStatus,
    /// Original labels retained for evidence targeting.
    pub labels: BTreeMap<String, String>,
    /// Original annotations retained for the report.
    pub annotations: BTreeMap<String, String>,
}

/// Converts an external alert into a stable, secret-free incident signal.
pub fn normalize(signal: AlertSignal) -> IncidentSignal {
    let alert_name = signal
        .labels
        .get("alertname")
        .cloned()
        .unwrap_or_else(|| "unnamed-alert".to_owned());
    let identity = if signal.fingerprint.trim().is_empty() {
        stable_label_hash(&signal.labels)
    } else {
        signal.fingerprint.clone()
    };
    IncidentSignal {
        correlation_id: identity.clone(),
        incident_id: format!("incident-{identity}"),
        episode: 1,
        alert_name,
        status: signal.status,
        labels: signal.labels,
        annotations: signal.annotations,
    }
}

fn stable_label_hash(labels: &BTreeMap<String, String>) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for (key, value) in labels {
        for byte in key
            .as_bytes()
            .iter()
            .chain([b'='].iter())
            .chain(value.as_bytes())
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= u64::from(b'\n');
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
