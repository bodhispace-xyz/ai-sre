//! Frozen Gate A replay corpus identities covering the required failure classes.

/// One deterministic replay case; evidence is supplied by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayFixture {
    /// Stable fixture identity.
    pub id: &'static str,
    /// Failure class used for like-for-like comparisons.
    pub class: &'static str,
}

/// Twenty representative cases, four in each declared failure class.
pub const CORPUS: [ReplayFixture; 20] = [
    ReplayFixture {
        id: "service-down-01",
        class: "service-down",
    },
    ReplayFixture {
        id: "service-down-02",
        class: "service-down",
    },
    ReplayFixture {
        id: "service-down-03",
        class: "service-down",
    },
    ReplayFixture {
        id: "service-down-04",
        class: "service-down",
    },
    ReplayFixture {
        id: "dependency-down-01",
        class: "dependency-down",
    },
    ReplayFixture {
        id: "dependency-down-02",
        class: "dependency-down",
    },
    ReplayFixture {
        id: "dependency-down-03",
        class: "dependency-down",
    },
    ReplayFixture {
        id: "dependency-down-04",
        class: "dependency-down",
    },
    ReplayFixture {
        id: "resource-saturation-01",
        class: "resource-saturation",
    },
    ReplayFixture {
        id: "resource-saturation-02",
        class: "resource-saturation",
    },
    ReplayFixture {
        id: "resource-saturation-03",
        class: "resource-saturation",
    },
    ReplayFixture {
        id: "resource-saturation-04",
        class: "resource-saturation",
    },
    ReplayFixture {
        id: "bad-deployment-01",
        class: "bad-deployment",
    },
    ReplayFixture {
        id: "bad-deployment-02",
        class: "bad-deployment",
    },
    ReplayFixture {
        id: "bad-deployment-03",
        class: "bad-deployment",
    },
    ReplayFixture {
        id: "bad-deployment-04",
        class: "bad-deployment",
    },
    ReplayFixture {
        id: "ambiguous-noisy-01",
        class: "ambiguous-noisy",
    },
    ReplayFixture {
        id: "ambiguous-noisy-02",
        class: "ambiguous-noisy",
    },
    ReplayFixture {
        id: "ambiguous-noisy-03",
        class: "ambiguous-noisy",
    },
    ReplayFixture {
        id: "ambiguous-noisy-04",
        class: "ambiguous-noisy",
    },
];

/// Checks the corpus shape without reading files or invoking providers.
pub fn is_gate_a_corpus() -> bool {
    let classes = CORPUS
        .iter()
        .map(|fixture| fixture.class)
        .collect::<std::collections::BTreeSet<_>>();
    CORPUS.len() >= 20 && classes.len() >= 5
}

#[cfg(test)]
mod tests {
    use super::{CORPUS, is_gate_a_corpus};

    #[test]
    fn corpus_has_required_size_and_classes() {
        // Given the frozen Gate A fixtures.
        // When the corpus contract is checked.
        // Then all declared failure classes are represented.
        assert_eq!(CORPUS.len(), 20);
        assert!(is_gate_a_corpus());
    }
}
