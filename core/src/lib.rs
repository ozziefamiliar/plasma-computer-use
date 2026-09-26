//! Core types for the Plasma computer-use backend.
//!
//! The backend is vision-first: the model sees a screenshot, emits pixel
//! coordinates *in that screenshot's coordinate space*, and the runtime maps
//! them back to the real desktop. Nothing here touches a backend; these are
//! the pure types that backends, the executor, and the MCP adapter share.
//!
//! ## The frame-ID contract (design decision, chew session #2)
//!
//! A coordinate action carries only `frame: FrameId` plus screenshot-space
//! pixel coordinates. The full geometry (`CoordSpace`) lives in the
//! executor-side [`FrameRegistry`], never in the action payload. Reasons:
//!
//! 1. **Stale coordinates must error, never silently map.** If geometry rode
//!    along in the payload, a model holding an old screenshot's coords could
//!    still "successfully" map them against stale geometry. Registry lookup
//!    failure is a hard `UnknownFrame` error instead.
//! 2. **Geometry is authoritative.** It is measured by the capture backend at
//!    capture time (downscale factor, fractional scale, crop origin). The
//!    model cannot corrupt or drift it.
//! 3. **Payloads stay small and CUA-shaped.** The model emits what it knows
//!    (pixels + frame id); the runtime owns what it measured.
//!
//! The registry is bounded (16 frames of *metadata* — pixels are evicted,
//! geometry is cheap) so a model can still reference a recent frame for
//! before/after comparison without unbounded growth.
//!
//! Transform chain: `PxPoint` (screenshot) --`CoordSpace`-->
//! `DesktopPoint` (absolute logical desktop px) --`DesktopGeometry`-->
//! `UInputAbs` (65535-normalized for the absolute uinput pointer).

pub mod action;
pub mod backend;
pub mod coord;
pub mod executor;
pub mod frame;
pub mod guard;
pub mod result;

pub use action::{Action, Batch, MouseButton};
pub use backend::{
    CapturedFrame, CaptureBackend, InputBackend, InputOp, MockCapture, MockInput, MockWindow,
    WindowBackend, WindowInfo,
};
pub use coord::{DesktopPoint, MapError, PxPoint, UInputAbs, map_desktop_to_uinput, map_px_to_desktop};
pub use executor::{Clock, Executor, MockClock, RealClock, Timing};
pub use frame::{CoordSpace, DesktopGeometry, FrameId, FrameMeta, FrameRegistry, ScreenshotDesc};
pub use guard::{Policy, Verdict, review, review_batch};
pub use result::{ActionOutcome, BatchResult, ExecError};
