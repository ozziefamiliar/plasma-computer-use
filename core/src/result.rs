//! Execution results and the error taxonomy.
//!
//! The recovery contract from the spec, encoded in types:
//!
//! - **Mapping failures are not retryable.** A stale frame or an out-of-frame
//!   point means the model's picture of the world is wrong; retrying would
//!   be inventing clicks. These go back to the model so it re-screenshots.
//! - **Infrastructure failures are retryable.** Dropped PipeWire stream,
//!   expired portal session, transient D-Bus error — the executor may retry
//!   these without model involvement.
//! - **A failed action never aborts its batch.** Outcomes are per-action.

use crate::coord::{DesktopPoint, MapError};
use crate::frame::FrameId;

/// Why an action failed, and whether the executor may retry it.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecError {
    /// Coordinate-mapping failure: stale frame or out-of-frame point.
    /// Never retried; returned to the model.
    Map(MapError),
    /// Infrastructure failure (dropped stream, expired portal session,
    /// transient D-Bus error). The executor may retry these.
    Infra(String),
    /// Backend-specific failure that is neither mapping nor known-transient
    /// (e.g. unresolvable key name). Returned to the model.
    Backend(String),
    /// Safety-guard denial or unconfirmed destructive action. Never retried;
    /// the reason is model-facing so the model can adjust.
    Guard(String),
}

impl ExecError {
    /// Whether the executor is allowed to retry without model involvement.
    pub fn retryable(&self) -> bool {
        matches!(self, ExecError::Infra(_))
    }
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Map(e) => write!(f, "{}", e),
            ExecError::Infra(e) => write!(f, "infrastructure failure: {}", e),
            ExecError::Backend(e) => write!(f, "backend failure: {}", e),
            ExecError::Guard(e) => write!(f, "safety guard: {}", e),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<MapError> for ExecError {
    fn from(e: MapError) -> Self {
        ExecError::Map(e)
    }
}

/// The outcome of one action inside a batch.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionOutcome {
    /// Executed. `mapped` holds every screenshot-space point the action used,
    /// mapped to absolute desktop coordinates — the debug log's "mapped
    /// desktop coordinates" field, and the audit trail.
    Done { mapped: Vec<DesktopPoint> },
    /// Executed but had no effect worth reporting (e.g. `Wait`).
    NoOp,
    /// Not executed (or partially executed) because of `error`.
    Failed { error: ExecError },
}

/// The result of one batch, outcomes parallel to the input batch's actions.
///
/// A `Screenshot` action additionally yields its new frame id so the caller
/// can correlate; the full self-describing payload comes from
/// `FrameMeta::describe`.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchResult {
    /// Per-action outcomes, in batch order.
    pub outcomes: Vec<ActionOutcome>,
    /// Frame ids registered by `Screenshot` actions in this batch.
    pub new_frames: Vec<FrameId>,
}

impl BatchResult {
    pub fn new() -> Self {
        Self {
            outcomes: Vec::new(),
            new_frames: Vec::new(),
        }
    }

    /// True if every action succeeded (no `Failed` outcomes).
    pub fn all_ok(&self) -> bool {
        !self
            .outcomes
            .iter()
            .any(|o| matches!(o, ActionOutcome::Failed { .. }))
    }
}

impl Default for BatchResult {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_infra_is_retryable() {
        assert!(!ExecError::Map(MapError::UnknownFrame(FrameId(1))).retryable());
        assert!(ExecError::Infra("pipewire dropped".into()).retryable());
        assert!(!ExecError::Backend("bad key name".into()).retryable());
    }

    #[test]
    fn all_ok_detects_failure() {
        let mut r = BatchResult::new();
        r.outcomes.push(ActionOutcome::NoOp);
        r.outcomes.push(ActionOutcome::Done { mapped: vec![] });
        assert!(r.all_ok());
        r.outcomes.push(ActionOutcome::Failed {
            error: ExecError::Backend("x".into()),
        });
        assert!(!r.all_ok());
    }
}
