//! Changes the pilot image scalar while preserving source bytes and verifying semantic scope.
//!
//! This low-level renderer accepts only the fixed image repository and immutable digest.
//! Its output is untrusted until deployment qualification and validation are satisfied.

use serde_yaml_ng::Value;
use thiserror::Error;

/// Fixed pilot path; callers cannot select another repository file.
pub const PILOT_PATH: &str = "stacks/utility/compose.yml";

/// A rejected repair requires fresh evidence or an operator recommendation, never a patch fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RenderError {
    /// The image does not name the fixed repository at an immutable digest.
    #[error("repair image is not an immutable pilot image")]
    InvalidImage,
    /// Unsupported, ambiguous, or excessive YAML must not be rewritten.
    #[error("Compose source cannot be safely rewritten")]
    UnsupportedSource,
    /// No repair is needed for an identical scalar.
    #[error("pilot image already matches")]
    NoChange,
}

/// Accepts a digest-only reference, without tags, interpolation, credentials, or alternate registries.
pub fn valid_image(image: &str) -> bool {
    image
        .strip_prefix("ghcr.io/corentinth/it-tools@sha256:")
        .is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// Returns source with exactly one scalar replaced; unsupported layouts fail closed.
pub fn render(source: &str, image: &str) -> Result<String, RenderError> {
    if !valid_image(image) {
        return Err(RenderError::InvalidImage);
    }
    if source.len() > 256 * 1024
        || source.contains('\t')
        || source
            .lines()
            .filter(|line| line.trim_start().starts_with("image:"))
            .count()
            > 32
    {
        return Err(RenderError::UnsupportedSource);
    }
    let mut expected: Value =
        serde_yaml_ng::from_str(source).map_err(|_| RenderError::UnsupportedSource)?;
    let slot = expected
        .get_mut("services")
        .and_then(|v| v.get_mut("it-tools"))
        .and_then(|v| v.get_mut("image"))
        .ok_or(RenderError::UnsupportedSource)?;
    if slot.as_str() == Some(image) {
        return Err(RenderError::NoChange);
    }
    if slot.as_str().is_none() {
        return Err(RenderError::UnsupportedSource);
    }
    *slot = Value::String(image.into());
    // A candidate must be a single-line image scalar. Parsing the whole candidate
    // proves the path rather than trusting indentation or a matching substring.
    let mut offset = 0;
    let mut accepted = None;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        if let Some(rest) = trimmed.strip_prefix("image:") {
            let leading = rest.len() - rest.trim_start_matches(' ').len();
            let scalar = &rest[leading..];
            let end = scalar.find(" #").unwrap_or(scalar.trim_end().len());
            if end > 0 && !scalar[..end].contains(['\n', '\r']) {
                let start = offset + line.len() - rest.len() + leading;
                let mut candidate = source.to_owned();
                candidate.replace_range(start..start + end, image);
                if serde_yaml_ng::from_str::<Value>(&candidate).ok().as_ref() == Some(&expected) {
                    if accepted.is_some() {
                        return Err(RenderError::UnsupportedSource);
                    }
                    accepted = Some(candidate);
                }
            }
        }
        offset += line.len();
    }
    accepted.ok_or(RenderError::UnsupportedSource)
}
