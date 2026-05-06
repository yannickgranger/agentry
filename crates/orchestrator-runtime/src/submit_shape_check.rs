//! Submit-time shape validation for `brief.payload.success_criteria`.
//!
//! Brief 84b-2 (closes #84). The preflight-criterion-agentry role consumes
//! `brief.payload.success_criteria` in the `cmd : expected` form
//! (space-colon-space separator). When a meta-brief targets a topology that
//! contains the preflight role, the criterion must be present and well-shaped
//! before the brief reaches Redis — otherwise preflight fails the run after
//! container spawn instead of the operator getting feedback at submit time.
//!
//! This module gates only topologies in [`TOPOLOGIES_WITH_SUCCESS_CRITERIA`].
//! Other topologies (work-briefs that carry `acceptance` instead) pass
//! through unchanged.
//!
//! Heuristics for the criterion content itself live in the preflight role —
//! refining them is a code-level change, not a runtime override.

use orchestrator_types::Brief;

/// Topology names whose roles consume `brief.payload.success_criteria`.
pub const TOPOLOGIES_WITH_SUCCESS_CRITERIA: &[&str] = &["agentry-planner-v0", "agentry-verify-v0"];

/// Separator the preflight role splits on — space-colon-space.
pub const CRITERION_SEPARATOR: &str = " : ";

/// Error returned when the shape check rejects a brief.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeError {
    /// `success_criteria` missing, empty, or whitespace-only.
    MissingOrEmpty,
    /// `success_criteria` does not contain ` : `.
    MissingSeparator,
    /// Right-hand side of ` : ` (after trimming) is empty.
    EmptyExpected,
}

impl ShapeError {
    /// Stable, operator-facing message printed to stderr by the CLI.
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            Self::MissingOrEmpty => {
                "error: brief.payload.success_criteria is required and cannot be empty for meta-briefs targeting agentry-planner-v0 or agentry-verify-v0"
            }
            Self::MissingSeparator => {
                "error: brief.payload.success_criteria must use the 'cmd : expected' format with space-colon-space separator"
            }
            Self::EmptyExpected => {
                "error: brief.payload.success_criteria 'expected' value cannot be empty"
            }
        }
    }
}

/// Run the shape check against a brief. Returns `Ok(())` either when the
/// brief targets a topology that doesn't use `success_criteria`, or when the
/// criterion is well-formed.
///
/// # Errors
/// Returns the matching [`ShapeError`] for the first violation found.
pub fn check_brief(brief: &Brief) -> Result<(), ShapeError> {
    if !TOPOLOGIES_WITH_SUCCESS_CRITERIA.contains(&brief.topology.name.as_str()) {
        return Ok(());
    }

    let criterion = brief
        .payload
        .get("success_criteria")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if criterion.trim().is_empty() {
        return Err(ShapeError::MissingOrEmpty);
    }

    let Some((_cmd, expected)) = criterion.split_once(CRITERION_SEPARATOR) else {
        return Err(ShapeError::MissingSeparator);
    };

    if expected.trim().is_empty() {
        return Err(ShapeError::EmptyExpected);
    }

    Ok(())
}
