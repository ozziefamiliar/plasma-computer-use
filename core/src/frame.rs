//! Frames: the unit of "what the model saw".
//!
//! A frame is a capture plus its measured coordinate space. The executor
//! keeps a bounded registry of frame metadata (geometry only — the image
//! bytes themselves are evicted and need not be retained for mapping).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Monotonic per-executor-session identifier for a captured frame.
///
/// Starts at 1; 0 is never issued. Survives only for the executor's lifetime —
/// frame ids are a session-local contract, not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameId(pub u64);

impl std::fmt::Display for FrameId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "frame#{}", self.0)
    }
}

/// How screenshot pixels map back to the desktop.
///
/// Captured at capture time by the capture backend; this is the authoritative
/// measurement. Covers every case the spec demands:
///
/// - **downscaling**: `scale_*` > 1 (desktop logical px per image px)
/// - **fractional scaling**: baked into `scale_*` (e.g. 1.25)
/// - **different X/Y scale after rounding**: `scale_x` and `scale_y` are
///   independent
/// - **cropped/window captures**: `origin_*` is the desktop position of the
///   image's top-left pixel
/// - **multi-monitor / negative origins**: `origin_*` may be negative; the
///   global [`DesktopGeometry`] handles the final normalization
///
/// Mapping: `desktop = origin + pixel * scale` (per axis).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CoordSpace {
    /// Width/height of the image the model actually saw, in pixels.
    pub image_w: u32,
    pub image_h: u32,
    /// Desktop logical position of the image's top-left pixel (may be
    /// negative for left/top monitors or cropped captures).
    pub origin_x: f64,
    pub origin_y: f64,
    /// Desktop logical px per image px, per axis.
    pub scale_x: f64,
    pub scale_y: f64,
}

/// The full logical desktop, as a bounding box over all monitors.
///
/// Needed for the final step into uinput's 65535-normalized absolute axes:
/// the compositor maps the normalized range onto the *global* desktop, so
/// negative monitor origins must be offset here, not per-frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DesktopGeometry {
    /// Logical position of the desktop's top-left corner (negative when a
    /// monitor sits left of or above the primary).
    pub min_x: f64,
    pub min_y: f64,
    /// Full logical extent of the desktop.
    pub width: f64,
    pub height: f64,
}

impl DesktopGeometry {
    /// Single-monitor, no-scaling desktop. The degenerate baseline.
    pub fn single(w: f64, h: f64) -> Self {
        Self {
            min_x: 0.0,
            min_y: 0.0,
            width: w,
            height: h,
        }
    }
}

/// A captured frame's metadata: id plus its measured coordinate space.
///
/// Returned to the model as the self-describing screenshot payload (via
/// [`FrameMeta::describe`]); stored in the [`FrameRegistry`] for coordinate
/// resolution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameMeta {
    pub id: FrameId,
    pub space: CoordSpace,
}

impl FrameMeta {
    /// The self-describing screenshot payload the model sees alongside the
    /// image bytes: frame id + image dims + scale, verbatim of the
    /// anaisbetts payload shape we decided to copy (session #1 notes).
    pub fn describe(&self) -> ScreenshotDesc {
        ScreenshotDesc {
            frame_id: self.id,
            width: self.space.image_w,
            height: self.space.image_h,
            scale_x: self.space.scale_x,
            scale_y: self.space.scale_y,
        }
    }
}

/// The model-facing description of a screenshot frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScreenshotDesc {
    pub frame_id: FrameId,
    pub width: u32,
    pub height: u32,
    pub scale_x: f64,
    pub scale_y: f64,
}

/// Bounded executor-side store of per-frame [`CoordSpace`] measurements.
///
/// This is where the frame-ID-bound coordinate contract lives: actions
/// reference frames by id; the registry resolves them. Lookup of an evicted
/// or never-issued id is a hard error (`MapError::UnknownFrame`), never a
/// silent best-effort map.
///
/// Bounded to [`MAX_FRAMES`] entries of cheap metadata; image bytes are not
/// stored here and may be evicted independently.
#[derive(Debug, Default)]
pub struct FrameRegistry {
    entries: VecDeque<(FrameId, CoordSpace)>,
    next: u64,
}

/// How many frames of geometry the registry retains.
pub const MAX_FRAMES: usize = 16;

impl FrameRegistry {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_FRAMES),
            next: 1,
        }
    }

    /// Register a freshly captured frame's coordinate space; returns its id.
    pub fn register(&mut self, space: CoordSpace) -> FrameId {
        let id = FrameId(self.next);
        self.next += 1;
        if self.entries.len() >= MAX_FRAMES {
            self.entries.pop_front();
        }
        self.entries.push_back((id, space));
        id
    }

    /// Look up a frame's coordinate space by id.
    pub fn lookup(&self, id: FrameId) -> Option<&CoordSpace> {
        self.entries.iter().find(|(fid, _)| *fid == id).map(|(_, s)| s)
    }

    /// Most recently registered frame, if any.
    pub fn latest(&self) -> Option<(FrameId, &CoordSpace)> {
        self.entries.back().map(|(id, s)| (*id, s))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> CoordSpace {
        CoordSpace {
            image_w: 1280,
            image_h: 720,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 1.5,
            scale_y: 1.5,
        }
    }

    #[test]
    fn ids_start_at_one_and_monotonically_increase() {
        let mut r = FrameRegistry::new();
        let a = r.register(space());
        let b = r.register(space());
        assert_eq!(a, FrameId(1));
        assert_eq!(b, FrameId(2));
        assert_ne!(a, b);
    }

    #[test]
    fn lookup_resolves_registered_space() {
        let mut r = FrameRegistry::new();
        let id = r.register(space());
        assert_eq!(r.lookup(id), Some(&space()));
    }

    #[test]
    fn lookup_of_unknown_id_returns_none() {
        let r = FrameRegistry::new();
        assert_eq!(r.lookup(FrameId(999)), None);
    }

    #[test]
    fn registry_is_bounded_and_evicts_oldest() {
        let mut r = FrameRegistry::new();
        let first = r.register(space());
        for _ in 0..MAX_FRAMES {
            r.register(space());
        }
        assert_eq!(r.len(), MAX_FRAMES);
        // The first frame has been evicted: stale coords against it must fail.
        assert_eq!(r.lookup(first), None);
    }

    #[test]
    fn describe_carries_self_describing_payload() {
        let mut r = FrameRegistry::new();
        let id = r.register(space());
        let meta = FrameMeta { id, space: space() };
        let desc = meta.describe();
        assert_eq!(desc.frame_id, id);
        assert_eq!((desc.width, desc.height), (1280, 720));
        assert_eq!((desc.scale_x, desc.scale_y), (1.5, 1.5));
    }
}
