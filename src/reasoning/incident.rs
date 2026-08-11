//! Alert normalization and in-process incident correlation.
//!
//! This module converts Alertmanager-shaped signals into a stable incident
//! identity before evidence collection or provider calls begin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An incoming alert or recovery notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// RFC3339 timestamp at which a firing alert started.
    #[serde(rename = "startsAt", default)]
    pub starts_at: String,
    /// RFC3339 timestamp at which a resolved alert ended.
    #[serde(rename = "endsAt", default)]
    pub ends_at: String,
    /// Alertmanager generator URL retained only as source metadata.
    #[serde(rename = "generatorURL", default)]
    pub generator_url: String,
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

/// Validates the semantic fields required for safe lifecycle correlation.
pub fn validate_webhook(webhook: &AlertmanagerWebhook) -> bool {
    webhook.truncated_alerts == 0
        && !webhook.alerts.is_empty()
        && webhook.alerts.iter().all(|alert| {
            let identity_present = !alert.fingerprint.trim().is_empty() || !alert.labels.is_empty();
            let timestamp = match alert.status {
                AlertStatus::Firing => &alert.starts_at,
                AlertStatus::Resolved => &alert.ends_at,
            };
            identity_present
                && !timestamp.trim().is_empty()
                && parse_rfc3339_millis(timestamp).is_some()
        })
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
    /// Source event time used to reject stale lifecycle updates.
    pub event_time: String,
    /// Stable source event identity for replay and diagnostics.
    pub source_event_id: String,
    /// Parsed UTC milliseconds used for chronological ordering.
    pub event_time_ms: Option<i128>,
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
    let event_time = match signal.status {
        AlertStatus::Firing => signal.starts_at.clone(),
        AlertStatus::Resolved => signal.ends_at.clone(),
    };
    let source_event_id = format!(
        "{}:{}:{}",
        identity,
        match signal.status {
            AlertStatus::Firing => "firing",
            AlertStatus::Resolved => "resolved",
        },
        event_time
    );
    let event_time_ms = parse_rfc3339_millis(&event_time);
    IncidentSignal {
        correlation_id: identity.clone(),
        incident_id: format!("incident-{identity}"),
        episode: 1,
        alert_name,
        status: signal.status,
        labels: signal.labels,
        annotations: signal.annotations,
        event_time,
        source_event_id,
        event_time_ms,
    }
}

/// Parses the Alertmanager RFC3339 timestamp subset into UTC milliseconds.
pub fn parse_rfc3339_millis(value: &str) -> Option<i128> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = digits(bytes, 0, 4)? as i64;
    let month = digits(bytes, 5, 2)? as i64;
    let day = digits(bytes, 8, 2)? as i64;
    let hour = digits(bytes, 11, 2)? as i64;
    let minute = digits(bytes, 14, 2)? as i64;
    let second = digits(bytes, 17, 2)? as i64;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut index = 19;
    let mut millis = 0_i64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if start == index {
            return None;
        }
        let fraction = std::str::from_utf8(&bytes[start..index]).ok()?;
        let mut digits = fraction.bytes().take(3).collect::<Vec<_>>();
        while digits.len() < 3 {
            digits.push(b'0');
        }
        millis = std::str::from_utf8(&digits).ok()?.parse().ok()?;
    }
    let offset_minutes = match bytes.get(index) {
        Some(b'Z') if index + 1 == bytes.len() => 0_i64,
        Some(b'+') | Some(b'-') => {
            let sign = if bytes[index] == b'+' { 1 } else { -1 };
            if index + 6 != bytes.len() || bytes.get(index + 3) != Some(&b':') {
                return None;
            }
            let hours = digits(bytes, index + 1, 2)? as i64;
            let minutes = digits(bytes, index + 4, 2)? as i64;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 60 + minutes)
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day)?;
    Some(
        (days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60) as i128 * 1_000
            + i128::from(millis),
    )
}

fn digits(bytes: &[u8], start: usize, length: usize) -> Option<u32> {
    let end = start.checked_add(length)?;
    let slice = bytes.get(start..end)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(slice).ok()?.parse().ok()
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let day_limit = match month {
        2 => 28 + i64::from((year % 4 == 0 && year % 100 != 0) || year % 400 == 0),
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if day > day_limit {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let month_index = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
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
