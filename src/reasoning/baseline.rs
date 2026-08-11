//! Deterministic incident enrichment used when model providers are unavailable.
//!
//! The baseline is deliberately pure: it derives an advisory report from the
//! alert identity and immutable evidence IDs without inventing a diagnosis or
//! granting action authority.

use std::collections::BTreeSet;

use super::contracts::{DiagnosticReport, EvidenceRef};

/// Builds a conservative, evidence-citing baseline report.
pub fn build_report(alert_name: &str, evidence_ids: &BTreeSet<String>) -> DiagnosticReport {
    DiagnosticReport {
        summary: format!(
            "Deterministic enrichment for {alert_name}; operator review is required before action."
        ),
        evidence: evidence_ids
            .iter()
            .cloned()
            .map(|evidence_id| EvidenceRef { evidence_id })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::build_report;

    #[test]
    fn baseline_is_stable_and_cites_only_supplied_evidence() {
        // Given an alert name and a deterministic set of evidence IDs.
        let evidence = BTreeSet::from(["evidence-0002".to_owned(), "evidence-0001".to_owned()]);

        // When the baseline report is built.
        let report = build_report("ApiDown", &evidence);

        // Then the report is conservative and citations are deterministic.
        assert!(report.summary.contains("ApiDown"));
        assert_eq!(
            report
                .evidence
                .iter()
                .map(|reference| reference.evidence_id.as_str())
                .collect::<Vec<_>>(),
            vec!["evidence-0001", "evidence-0002"]
        );
    }
}
