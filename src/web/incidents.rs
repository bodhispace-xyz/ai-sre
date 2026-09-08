//! Bounded, escaped incident-page rendering with no mutation controls.

use std::{collections::BTreeMap, sync::Arc};

use tokio::sync::RwLock;

use crate::reasoning::investigation::InvestigationResult;

/// Maximum number of completed reports retained for operator reads.
pub const MAX_STORED_REPORTS: usize = 256;

/// Bounded in-process projection of reports for the read-only page.
#[derive(Clone, Default)]
pub struct IncidentPages(Arc<RwLock<BTreeMap<String, String>>>);

impl IncidentPages {
    /// Stores the latest report and evicts oldest keys at the bound.
    pub async fn put(&self, result: InvestigationResult) {
        let mut reports = self.0.write().await;
        let incident_id = result.incident_id.clone();
        reports.insert(incident_id, render(&result));
        while reports.len() > MAX_STORED_REPORTS {
            let Some(key) = reports.keys().next().cloned() else {
                break;
            };
            reports.remove(&key);
        }
    }

    /// Returns a report only for an exact incident identity.
    pub async fn get(&self, incident_id: &str) -> Option<String> {
        self.0.read().await.get(incident_id).cloned()
    }

    /// Restores a previously rendered report without reintroducing raw payloads.
    pub async fn put_rendered(&self, incident_id: String, rendered: String) {
        let mut reports = self.0.write().await;
        reports.insert(incident_id, rendered);
        while reports.len() > MAX_STORED_REPORTS {
            let Some(key) = reports.keys().next().cloned() else {
                break;
            };
            reports.remove(&key);
        }
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
    let trace_id = crate::observability::tracing::incident_trace_id(&result.incident_id);
    let summary = escape_html(&crate::reasoning::investigation::redact_text(
        &result.report.summary,
    ));
    let provider = crate::reasoning::investigation::ShadowInvestigator::terminal_provider(result)
        .map_or_else(
            || "deterministic-baseline".to_owned(),
            |provider| format!("{provider:?}"),
        );
    let citations = result
        .report
        .evidence
        .iter()
        .map(|evidence| format!("<li>{}</li>", escape_html(&evidence.evidence_id)))
        .collect::<String>();
    let report = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>AI SRE incident</title></head><body><main><h1>Incident {}</h1><p role=\"status\">{summary}</p><p>Status: {:?}</p><p>Provider: {provider}</p><p>Evidence records: {}</p><ul aria-label=\"Evidence citations\">{citations}</ul><p>Efficiency and cost are reconstructed from the durable journal. Mode: shadow; no action controls are available.</p></main></body></html>",
        escape_html(&result.incident_id),
        result.status,
        result.evidence_ids.len()
    );
    report.replace("</main>", &format!("<p>Tempo trace ID: <code>{trace_id}</code>. Paste into Grafana Explore with the Tempo datasource. A trace may be unavailable if export was disabled, dropped, or expired.</p></main>"))
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
