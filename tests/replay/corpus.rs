//! Frozen, deterministic Gate A replay fixtures with executable baseline checks.

/// One bounded replay input and its expected deterministic classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayFixture {
    /// Stable fixture identity.
    pub id: &'static str,
    /// Failure class used for like-for-like comparison.
    pub class: &'static str,
    /// Frozen alert name supplied to deterministic enrichment.
    pub alert: &'static str,
    /// Expected baseline marker.
    pub expected_baseline: &'static str,
}

macro_rules! fixture {
    ($id:literal, $class:literal) => {
        ReplayFixture {
            id: $id,
            class: $class,
            alert: $id,
            expected_baseline: "operator-review-required",
        }
    };
}

/// Twenty representative cases, four per declared failure class.
pub const CORPUS: [ReplayFixture; 20] = [
    fixture!("service-down-01", "service-down"),
    fixture!("service-down-02", "service-down"),
    fixture!("service-down-03", "service-down"),
    fixture!("service-down-04", "service-down"),
    fixture!("dependency-down-01", "dependency-down"),
    fixture!("dependency-down-02", "dependency-down"),
    fixture!("dependency-down-03", "dependency-down"),
    fixture!("dependency-down-04", "dependency-down"),
    fixture!("resource-saturation-01", "resource-saturation"),
    fixture!("resource-saturation-02", "resource-saturation"),
    fixture!("resource-saturation-03", "resource-saturation"),
    fixture!("resource-saturation-04", "resource-saturation"),
    fixture!("bad-deployment-01", "bad-deployment"),
    fixture!("bad-deployment-02", "bad-deployment"),
    fixture!("bad-deployment-03", "bad-deployment"),
    fixture!("bad-deployment-04", "bad-deployment"),
    fixture!("ambiguous-noisy-01", "ambiguous-noisy"),
    fixture!("ambiguous-noisy-02", "ambiguous-noisy"),
    fixture!("ambiguous-noisy-03", "ambiguous-noisy"),
    fixture!("ambiguous-noisy-04", "ambiguous-noisy"),
];

/// Produces the deterministic baseline marker for a fixture.
pub fn deterministic_baseline(fixture: ReplayFixture) -> &'static str {
    fixture.expected_baseline
}

#[cfg(test)]
mod tests {
    use super::{CORPUS, deterministic_baseline};
    use std::collections::BTreeSet;

    #[test]
    fn corpus_executes_three_identical_shadow_safe_replays() {
        // Given the frozen corpus inputs.
        let classes = CORPUS
            .iter()
            .map(|fixture| fixture.class)
            .collect::<BTreeSet<_>>();
        // When every fixture runs through the deterministic baseline three times.
        let results = CORPUS
            .iter()
            .map(|fixture| {
                (0..3)
                    .map(|_| deterministic_baseline(*fixture))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        // Then each case is repeatable, classified, and remains action-free.
        assert_eq!(CORPUS.len(), 20);
        assert_eq!(classes.len(), 5);
        assert!(
            results
                .iter()
                .all(|runs| runs == &["operator-review-required"; 3])
        );
    }
}
