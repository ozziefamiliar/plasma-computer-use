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
use std::time::Duration;

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
    /// Safety-guard denial. Never retried; the reason is model-facing so the
    /// model can adjust.
    Guard(String),
    /// Guard verdict that needs a human nod. Never retried by the executor;
    /// the reason is model-facing. Renders identically to the pre-channel
    /// string so the MCP wire is unchanged. An action reaches this state
    /// only when no confirmation was granted for it (see
    /// [`crate::Executor::grant_confirmation`] and
    /// [`crate::Executor::set_confirm_hook`]).
    NeedsConfirm { reason: String },
    /// Emergency cancellation: the cancel flag was armed (via
    /// [`crate::Executor::cancel`], typically from an MCP
    /// `notifications/cancelled`) before this action ran. Never retried;
    /// remaining actions in the batch are drained as cancelled too.
    Cancelled(String),
    /// Rate limit exceeded: the action-rate bucket was empty. Never retried
    /// by the executor — the model must observe the limit and slow down.
    /// `retry_after` is the executor's estimate of when one token will be
    /// available; `None` means no refill is configured, so the limit will
    /// not clear on its own.
    RateLimited {
        retry_after: Option<Duration>,
    },
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
            ExecError::NeedsConfirm { reason } => write!(f, "confirmation required: {}", reason),
            ExecError::Cancelled(e) => write!(f, "cancelled: {}", e),
            ExecError::RateLimited { retry_after } => match retry_after {
                Some(wait) => write!(
                    f,
                    "rate limited: retry after {:.1}s",
                    wait.as_secs_f64()
                ),
                None => write!(f, "rate limited: bucket exhausted, no refill configured"),
            },
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
    /// A window action's result. `windows` carries the serializable window
    /// descriptions (for list/find/active/bounds); `focused` is `Some` only
    /// for `FocusWindow` — `false` means the backend didn't know the id, so
    /// nothing was touched.
    Windows {
        windows: Vec<crate::backend::WindowInfo>,
        focused: Option<bool>,
    },
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
    /// Mouse buttons the executor found still pressed at a batch boundary
    /// and released during its end-of-batch stuck-input sweep. Empty is the
    /// common case: non-empty means some action's release failed after
    /// retries and the sweep cleaned it up.
    pub stuck_released: Vec<String>,
    /// Buttons the sweep could *not* release — per-button release and the
    /// backend's `release_all` both failed. The desktop may genuinely have
    /// a stuck button; surfaced on the wire so the model knows.
    pub stuck_unreleased: Vec<String>,
}

impl BatchResult {
    pub fn new() -> Self {
        Self {
            outcomes: Vec::new(),
            new_frames: Vec::new(),
            stuck_released: Vec::new(),
            stuck_unreleased: Vec::new(),
        }
    }

    /// (action index, model-facing reason) for every action in this batch
    /// that failed as [`ExecError::NeedsConfirm`]. Hosts use this to build
    /// the operator prompt, then re-submit the approved actions with
    /// [`crate::Executor::grant_confirmation`] armed (or rely on the
    /// executor's confirm hook, see
    /// [`crate::Executor::set_confirm_hook`]).
    pub fn pending_confirmations(&self) -> Vec<(usize, String)> {
        self.outcomes
            .iter()
            .enumerate()
            .filter_map(|(i, o)| match o {
                ActionOutcome::Failed {
                    error: ExecError::NeedsConfirm { reason },
                } => Some((i, reason.clone())),
                _ => None,
            })
            .collect()
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
        assert!(!ExecError::Guard("denied".into()).retryable());
        assert!(!ExecError::Cancelled("emergency stop".into()).retryable());
        assert!(!ExecError::RateLimited {
            retry_after: Some(Duration::from_secs(1))
        }
        .retryable());
    }

    #[test]
    fn rate_limited_renders_model_readable() {
        assert_eq!(
            ExecError::RateLimited {
                retry_after: Some(Duration::from_millis(1200))
            }
            .to_string(),
            "rate limited: retry after 1.2s"
        );
        assert_eq!(
            ExecError::RateLimited { retry_after: None }.to_string(),
            "rate limited: bucket exhausted, no refill configured"
        );
    }

    #[test]
    fn cancelled_renders_model_readable() {
        assert_eq!(
            ExecError::Cancelled("emergency stop".into()).to_string(),
            "cancelled: emergency stop"
        );
    }

    #[test]
    fn needs_confirm_renders_wire_compatible() {
        // Same string the executor used to build by hand before the
        // confirmation channel existed: the MCP wire is unchanged.
        assert_eq!(
            ExecError::NeedsConfirm {
                reason: "typed text contains destructive fragment \"rm -rf\"".into()
            }
            .to_string(),
            "confirmation required: typed text contains destructive fragment \"rm -rf\""
        );
    }

    #[test]
    fn needs_confirm_is_not_retryable() {
        assert!(!ExecError::NeedsConfirm {
            reason: "x".into()
        }
        .retryable());
    }

    #[test]
    fn pending_confirmations_lists_index_and_reason() {
        let mut r = BatchResult::new();
        r.outcomes.push(ActionOutcome::NoOp);
        r.outcomes.push(ActionOutcome::Failed {
            error: ExecError::NeedsConfirm {
                reason: "destructive fragment".into(),
            },
        });
        r.outcomes.push(ActionOutcome::Failed {
            error: ExecError::Guard("denied".into()),
        });
        assert_eq!(
            r.pending_confirmations(),
            vec![(1, "destructive fragment".into())]
        );
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
