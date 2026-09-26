//! The model-facing action schema.
//!
//! CUA-aligned, batched, and deliberately thin: coordinate actions carry
//! screenshot-space pixels plus a `frame` id; geometry resolution happens in
//! the executor against the [`FrameRegistry`](crate::frame::FrameRegistry).
//! Nothing MCP-specific leaks in here.

use crate::coord::PxPoint;
use crate::frame::FrameId;
use serde::{Deserialize, Serialize};

/// Ordered batch of actions executed in sequence by one executor call.
///
/// Per-action outcomes are independent: a failed action does not abort the
/// rest of the batch (the anaisbetts lesson). A `Screenshot` mid-batch
/// registers a new frame that later actions in the *same* batch may
/// reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch(pub Vec<Action>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// One physical action. Serializes to the JSON shape in the spec, e.g.
/// `{"type":"click","frame":3,"button":"left","x":400,"y":200}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Capture a new screenshot; registers a frame and returns its
    /// self-describing payload. The optional note is free-form context for
    /// the audit/debug log.
    Screenshot { note: Option<String> },

    /// Move the cursor to a screenshot-space point (no click).
    Move { frame: FrameId, x: u32, y: u32 },

    /// Press and release a button at a screenshot-space point.
    Click {
        frame: FrameId,
        button: MouseButton,
        x: u32,
        y: u32,
    },

    /// Two clicks at a screenshot-space point.
    DoubleClick {
        frame: FrameId,
        button: MouseButton,
        x: u32,
        y: u32,
    },

    /// Press at the first point, interpolate through the rest, release.
    /// All points share one frame: the path was planned from one screenshot.
    Drag { frame: FrameId, path: Vec<PxPoint> },

    /// Scroll at a screenshot-space point. `dx`/`dy` are in scroll ticks
    /// (positive = right/down, matching the spec's `scroll_y: 600`).
    Scroll {
        frame: FrameId,
        x: u32,
        y: u32,
        dx: f64,
        dy: f64,
    },

    /// Press a key combination, e.g. `["CTRL","L"]`. Key names are resolved
    /// by the input backend (keysyms, layout-independent); unresolvable names
    /// are a backend error, not a silent no-op.
    Keypress { keys: Vec<String> },

    /// Insert literal Unicode text. The backend picks the best layout-aware
    /// path (keysyms / libei / clipboard-paste fallback); it must never
    /// silently drop characters outside the active layout.
    Type { text: String },

    /// Sleep before the next action in the batch.
    Wait { ms: u64 },
}

impl Action {
    /// The frame this action's coordinates are bound to, if any.
    pub fn frame(&self) -> Option<FrameId> {
        match self {
            Action::Move { frame, .. }
            | Action::Click { frame, .. }
            | Action::DoubleClick { frame, .. }
            | Action::Drag { frame, .. }
            | Action::Scroll { frame, .. } => Some(*frame),
            Action::Screenshot { .. } | Action::Keypress { .. } | Action::Type { .. } | Action::Wait { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_json_matches_spec_shape() {
        // The spec's example payloads must deserialize as-is.
        let click: Action =
            serde_json::from_str(r#"{"type":"click","frame":3,"button":"left","x":400,"y":200}"#)
                .unwrap();
        assert_eq!(
            click,
            Action::Click {
                frame: FrameId(3),
                button: MouseButton::Left,
                x: 400,
                y: 200,
            }
        );

        let drag: Action = serde_json::from_str(
            r#"{"type":"drag","frame":1,"path":[{"x":100,"y":100},{"x":300,"y":300}]}"#,
        )
        .unwrap();
        assert_eq!(
            drag,
            Action::Drag {
                frame: FrameId(1),
                path: vec![
                    PxPoint { x: 100, y: 100 },
                    PxPoint { x: 300, y: 300 },
                ],
            }
        );

        let batch: Batch = serde_json::from_str(
            r#"[{"type":"screenshot","note":null},{"type":"wait","ms":500},{"type":"type","text":"hello"}]"#,
        )
        .unwrap();
        assert_eq!(batch.0.len(), 3);
    }

    #[test]
    fn frame_binding_extraction() {
        assert_eq!(
            Action::Move {
                frame: FrameId(7),
                x: 1,
                y: 1
            }
            .frame(),
            Some(FrameId(7))
        );
        assert_eq!(Action::Wait { ms: 10 }.frame(), None);
        assert_eq!(
            Action::Screenshot { note: None }.frame(),
            None
        );
    }
}
