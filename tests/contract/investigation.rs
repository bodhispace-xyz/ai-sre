//! GIVEN/WHEN/THEN contracts for shadow investigation query planning.

use ai_sre::reasoning::investigation::ShadowInvestigator;

#[test]
fn query_planning_allows_only_safe_service_identifiers() {
    // Given a normal service name and an injection-shaped service name.
    let safe = "api-gateway";
    let unsafe_name = "api\"} | rm -rf /";

    // When conservative investigation queries are built.
    let queries = ShadowInvestigator::queries_for_service(safe).expect("safe service");
    let rejected = ShadowInvestigator::queries_for_service(unsafe_name);

    // Then the generated expressions target only the selected service and unsafe input is rejected.
    assert_eq!(queries.logs, r#"{service="api-gateway"} |= "error""#);
    assert_eq!(queries.metrics, r#"up{service="api-gateway"}"#);
    assert!(rejected.is_none());
}
