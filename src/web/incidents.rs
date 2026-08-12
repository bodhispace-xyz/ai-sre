//! Bounded, escaped incident-page rendering with no mutation controls.

use crate::reasoning::investigation::InvestigationResult;

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
    use super::render;
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
}
