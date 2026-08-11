//! GIVEN/WHEN/THEN contracts for fail-closed deployment configuration loading.

use ai_sre::config::{AppConfig, ConfigLoadError};

#[test]
fn config_loader_accepts_complete_json_and_rejects_unknown_policy() {
    // Given a complete default configuration and a configuration with an unknown field.
    let valid = serde_json::to_string(&AppConfig::default()).expect("serialize default");
    let invalid = valid.replace('}', ",\"unknown\":true}");

    // When both documents cross the configuration boundary.
    let loaded = AppConfig::from_json(&valid);
    let rejected = AppConfig::from_json(&invalid);

    // Then valid policy loads and unknown fields fail closed.
    assert!(loaded.is_ok());
    assert!(matches!(rejected, Err(ConfigLoadError::Parse(_))));
}

#[test]
fn config_loader_rejects_unknown_nested_reasoning_policy() {
    // Given a valid configuration with an unknown nested budget field.
    let valid = serde_json::to_string(&AppConfig::default()).expect("serialize default");
    let invalid = valid.replace(
        "\"max_wall_time_secs\":300",
        "\"max_wall_time_secs\":300,\"max_wall_time_seconds\":300",
    );

    // When the nested policy crosses deserialization.
    let result = AppConfig::from_json(&invalid);

    // Then the typo cannot silently change runtime policy.
    assert!(matches!(result, Err(ConfigLoadError::Parse(_))));
}
