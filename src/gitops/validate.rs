//! Verifies exact renderer output and base identity before any sandbox execution or handoff.
//!
//! These pure checks do not execute repository scripts or authenticate repository
//! evidence. The read-only adapter must supply the actual changed-file set and SHAs.

use super::it_tools_image::{PILOT_PATH, render};
use thiserror::Error;

/// Scope failures never authorize force-push, conflict resolution, or validator bypass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ScopeError {
    /// Revision identifiers are malformed or the base has moved.
    #[error("repository base changed or is invalid")]
    BaseChanged,
    /// The candidate differs from the deterministic one-field repair.
    #[error("repair exceeds the pilot scope")]
    OutsideScope,
}

/// Checks the exact candidate bytes against a fresh base revision and complete path list.
/// Passing this check does not imply that sandboxed validation has run.
pub fn validate_change(
    expected_base: &str,
    current_base: &str,
    source: &str,
    candidate: &str,
    image: &str,
    changed_paths: &[&str],
) -> Result<(), ScopeError> {
    if !valid_sha(expected_base) || expected_base != current_base {
        return Err(ScopeError::BaseChanged);
    }
    if changed_paths != [PILOT_PATH]
        || candidate.len() > 256 * 1024
        || render(source, image).as_deref() != Ok(candidate)
    {
        return Err(ScopeError::OutsideScope);
    }
    Ok(())
}

pub(crate) fn valid_sha(value: &str) -> bool {
    [40, 64].contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
