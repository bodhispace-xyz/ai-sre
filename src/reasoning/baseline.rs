//! Deterministic incident enrichment used when model providers are unavailable.
//!
//! The baseline is deliberately pure: it derives an advisory report from the
//! alert identity and immutable evidence IDs without inventing a diagnosis or
//! granting action authority.

use std::collections::BTreeSet;

use super::contracts::{DiagnosticReport, EvidenceRef};

/// Deterministic context assembled before any provider is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterministicEnrichment {
    /// Normalized alert identity.
    pub alert_name: String,
    /// Evidence already committed before model reasoning.
    pub evidence_ids: Vec<String>,
    /// Version-controlled runbook references applicable to the alert.
    pub runbook_links: Vec<String>,
}

impl DeterministicEnrichment {
    /// Creates bounded enrichment from trusted alert facts and evidence IDs.
    pub fn new(
        alert_name: impl Into<String>,
        evidence_ids: impl IntoIterator<Item = String>,
        runbook_links: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            alert_name: alert_name.into(),
            evidence_ids: evidence_ids.into_iter().take(64).collect(),
            runbook_links: runbook_links.into_iter().take(16).collect(),
        }
    }

    /// Renders the deterministic facts without model-authored instructions.
    pub fn render(&self) -> String {
        let mut output = format!("Alert: {}\nEvidence IDs:\n", self.alert_name);
        for evidence_id in &self.evidence_ids {
            output.push_str("- ");
            output.push_str(evidence_id);
            output.push('\n');
        }
        output.push_str("Runbooks:\n");
        for link in &self.runbook_links {
            output.push_str("- ");
            output.push_str(link);
            output.push('\n');
        }
        output
    }
}

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

    use super::DeterministicEnrichment;
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

    #[test]
    fn deterministic_enrichment_is_bounded_and_model_neutral() {
        // Given more evidence and runbook links than the enrichment contract permits.
        let enrichment = DeterministicEnrichment::new(
            "ApiDown",
            (0..100).map(|index| format!("evidence-{index:04}")),
            (0..30).map(|index| format!("runbook-{index}")),
        );

        // When the trusted seed pack is rendered.
        let rendered = enrichment.render();

        // Then it remains bounded and contains no provider-authored instructions.
        assert_eq!(enrichment.evidence_ids.len(), 64);
        assert_eq!(enrichment.runbook_links.len(), 16);
        assert!(rendered.starts_with("Alert: ApiDown"));
    }
}
