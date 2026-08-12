//! Bounded, escaped incident-page rendering with no mutation controls.

use std::{collections::BTreeMap, sync::Arc};

use tokio::sync::RwLock;

use crate::reasoning::investigation::InvestigationResult;

/// Maximum number of completed reports retained for operator reads.
pub const MAX_STORED_REPORTS: usize = 256;

/// Bounded in-process projection of reports for the read-only page.
#[derive(Clone, Default)]
pub struct IncidentPages(Arc<RwLock<BTreeMap<String, InvestigationResult>>>);

impl IncidentPages {
    /// Stores the latest report and evicts oldest keys at the bound.
    pub async fn put(&self, result: InvestigationResult) {
        let mut reports = self.0.write().await;
        reports.insert(result.incident_id.clone(), result);
        while reports.len() > MAX_STORED_REPORTS {
            let Some(key) = reports.keys().next().cloned() else {
                break;
            };
            reports.remove(&key);
        }
    }

    /// Returns a report only for an exact incident identity.
    pub async fn get(&self, incident_id: &str) -> Option<InvestigationResult> {
        self.0.read().await.get(incident_id).cloned()
    }
}

/// Security headers required on the read-only incident page.
pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("Cache-Control", "no-store"),
    ("Referrer-Policy", "no-referrer"),
    (
        "Content-Security-Policy",
        "default-src 'none'; style-src 'unsafe-inline'",
    ),
];

/// Renders a small accessible report page from already-redacted state.
pub fn render(result: &InvestigationResult) -> String {
    let summary = escape_html(&result.report.summary);
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>AI SRE incident</title></head><body><main><h1>Incident {}</h1><p role=\"status\">{summary}</p><p>Evidence records: {}</p><p>Mode: shadow; no action controls are available.</p></main></body></html>",
        escape_html(&result.incident_id),
        result.evidence_ids.len()
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[cfg(test)]
mod tests {
    use super::{IncidentPages, MAX_STORED_REPORTS, render};
    use crate::reasoning::{
        contracts::{DiagnosticReport, EvidenceRef},
        investigation::InvestigationResult,
    };

    #[test]
    fn incident_page_escapes_untrusted_summary_and_has_no_controls() {
        // Given an incident summary containing markup-like provider text.
        let result = InvestigationResult {
            incident_id: "incident-1".to_owned(),
            evidence_ids: std::collections::BTreeSet::from(["evidence-0001".to_owned()]),
            status: crate::reasoning::coordinator::RunStatus::Exhausted,
            report: DiagnosticReport {
                summary: "<script>alert(1)</script>".to_owned(),
                evidence: vec![EvidenceRef {
                    evidence_id: "evidence-0001".to_owned(),
                }],
            },
        };

        // When the read-only incident page is rendered.
        let page = render(&result);

        // Then markup is inert and mutation controls are absent.
        assert!(!page.contains("<script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("approve"));
    }

    #[tokio::test]
    async fn incident_pages_are_bounded_and_exactly_addressable() {
        // Given more reports than the in-process page projection permits.
        let pages = IncidentPages::default();
        for index in 0..=MAX_STORED_REPORTS {
            pages
                .put(InvestigationResult {
                    incident_id: format!("incident-{index:04}"),
                    evidence_ids: std::collections::BTreeSet::new(),
                    status: crate::reasoning::coordinator::RunStatus::Exhausted,
                    report: DiagnosticReport {
                        summary: "bounded".to_owned(),
                        evidence: Vec::new(),
                    },
                })
                .await;
        }

        // When an operator requests an exact incident identity.
        let retained = pages.get("incident-0256").await;
        let evicted = pages.get("incident-0000").await;

        // Then the newest report is available and older state is evicted.
        assert!(retained.is_some());
        assert!(evicted.is_none());
    }
}
