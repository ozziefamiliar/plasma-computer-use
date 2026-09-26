//! Coordinate points and the transforms between them.
//!
//! All transforms are pure functions of measured geometry — no backend I/O,
//! no ambient state. This is the module the spec's "strong unit tests" demand
//! lives in.

use crate::frame::{CoordSpace, DesktopGeometry, FrameId};
use serde::{Deserialize, Serialize};

/// A pixel in the coordinate space of a specific screenshot frame.
///
/// Integer: the model points at pixels, and the capture backend reports
/// integer image dimensions. Carries no frame id itself — actions bind points
/// to frames; see the contract in the crate docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PxPoint {
    pub x: u32,
    pub y: u32,
}

/// An absolute position on the logical desktop, in logical pixels.
///
/// This is what backends consume (before their own normalization to uinput
/// axes, RemoteDesktop coordinates, etc.). Fractional by design: fractional
/// scaling and downscaling both land between pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DesktopPoint {
    pub x: f64,
    pub y: f64,
}

/// A position normalized for the absolute uinput pointer's 0..=65535 axes.
///
/// The compositor maps this range onto the *global* desktop, so negative
/// monitor origins are folded in at construction, never left to the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UInputAbs {
    pub x: u16,
    pub y: u16,
}

/// Map a screenshot-space pixel to the absolute logical desktop.
///
/// The point is resolved against the given frame's measured [`CoordSpace`].
/// A point outside the image bounds is an [`MapError::OutOfFrame`] — the
/// runtime must *not* execute it and must *not* invent a nearby click; the
/// error goes back to the model so it can re-screenshot and reassess.
pub fn map_px_to_desktop(
    frame: FrameId,
    space: &CoordSpace,
    p: PxPoint,
) -> Result<DesktopPoint, MapError> {
    if p.x >= space.image_w || p.y >= space.image_h {
        return Err(MapError::OutOfFrame {
            frame,
            x: p.x,
            y: p.y,
            image_w: space.image_w,
            image_h: space.image_h,
        });
    }
    Ok(DesktopPoint {
        x: space.origin_x + p.x as f64 * space.scale_x,
        y: space.origin_y + p.y as f64 * space.scale_y,
    })
}

/// Map an absolute desktop point to uinput's 65535-normalized absolute axes.
///
/// Points outside the desktop bounds are **clamped**, not rejected: a model
/// aiming at the outermost pixel must still produce a legal axis value (the
/// uinput driver would wrap or error on out-of-range values). Rounding is to
/// nearest, halves away from zero.
pub fn map_desktop_to_uinput(geo: &DesktopGeometry, d: DesktopPoint) -> UInputAbs {
    debug_assert!(geo.width > 0.0 && geo.height > 0.0);
    let nx = ((d.x - geo.min_x) / geo.width).clamp(0.0, 1.0);
    let ny = ((d.y - geo.min_y) / geo.height).clamp(0.0, 1.0);
    UInputAbs {
        x: (nx * 65535.0).round() as u16,
        y: (ny * 65535.0).round() as u16,
    }
}

/// Coordinate-mapping failures. None of these are retryable: retrying a
/// stale or out-of-frame coordinate is exactly the "invent nearby clicks"
/// behavior the spec forbids.
#[derive(Debug, Clone, PartialEq)]
pub enum MapError {
    /// The referenced frame is unknown or evicted — stale coordinates.
    UnknownFrame(FrameId),
    /// The point lies outside the screenshot the model saw.
    OutOfFrame {
        frame: FrameId,
        x: u32,
        y: u32,
        image_w: u32,
        image_h: u32,
    },
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MapError::UnknownFrame(id) => write!(
                f,
                "stale coordinates: {} is unknown or evicted; take a new screenshot",
                id
            ),
            MapError::OutOfFrame {
                frame,
                x,
                y,
                image_w,
                image_h,
            } => write!(
                f,
                "point ({},{}) is outside {} ({}x{}); take a new screenshot",
                x, y, frame, image_w, image_h
            ),
        }
    }
}

impl std::error::Error for MapError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fid() -> FrameId {
        FrameId(1)
    }

    #[test]
    fn identity_map_single_monitor_no_scaling() {
        let space = CoordSpace {
            image_w: 1920,
            image_h: 1080,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        };
        let d = map_px_to_desktop(fid(), &space, PxPoint { x: 400, y: 200 }).unwrap();
        assert_eq!(d, DesktopPoint { x: 400.0, y: 200.0 });
    }

    #[test]
    fn downscaled_capture_maps_up() {
        // 3840x2160 desktop captured at 1280x720: 3 desktop px per image px.
        let space = CoordSpace {
            image_w: 1280,
            image_h: 720,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 3.0,
            scale_y: 3.0,
        };
        let d = map_px_to_desktop(fid(), &space, PxPoint { x: 400, y: 200 }).unwrap();
        assert_eq!(d, DesktopPoint { x: 1200.0, y: 600.0 });
    }

    #[test]
    fn fractional_scaling() {
        // 150% scale: 2560 logical -> 3840 physical, captured at logical size.
        let space = CoordSpace {
            image_w: 2560,
            image_h: 1440,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 1.5,
            scale_y: 1.5,
        };
        let d = map_px_to_desktop(fid(), &space, PxPoint { x: 100, y: 100 }).unwrap();
        assert_eq!(d, DesktopPoint { x: 150.0, y: 150.0 });
    }

    #[test]
    fn different_xy_scale_after_rounding() {
        // Mixed-DPI capture where the downscale factor rounds differently
        // per axis — must stay independent.
        let space = CoordSpace {
            image_w: 1280,
            image_h: 720,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 2.0,
            scale_y: 2.25,
        };
        let d = map_px_to_desktop(fid(), &space, PxPoint { x: 100, y: 100 }).unwrap();
        assert_eq!(d, DesktopPoint { x: 200.0, y: 225.0 });
    }

    #[test]
    fn multi_monitor_negative_origin() {
        // Second monitor sits left of primary: image covers the left monitor
        // whose desktop origin is at x = -1920.
        let space = CoordSpace {
            image_w: 1920,
            image_h: 1080,
            origin_x: -1920.0,
            origin_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        };
        let d = map_px_to_desktop(fid(), &space, PxPoint { x: 100, y: 50 }).unwrap();
        assert_eq!(d, DesktopPoint { x: -1820.0, y: 50.0 });
    }

    #[test]
    fn cropped_window_capture_offsets_origin() {
        // Window capture: top-left of the image is at desktop (640, 360).
        let space = CoordSpace {
            image_w: 800,
            image_h: 600,
            origin_x: 640.0,
            origin_y: 360.0,
            scale_x: 1.0,
            scale_y: 1.0,
        };
        let d = map_px_to_desktop(fid(), &space, PxPoint { x: 0, y: 0 }).unwrap();
        assert_eq!(d, DesktopPoint { x: 640.0, y: 360.0 });
        let d = map_px_to_desktop(fid(), &space, PxPoint { x: 799, y: 599 }).unwrap();
        assert_eq!(d, DesktopPoint { x: 1439.0, y: 959.0 });
    }

    #[test]
    fn out_of_frame_is_an_error_not_a_clamp() {
        let space = CoordSpace {
            image_w: 1280,
            image_h: 720,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        };
        let err = map_px_to_desktop(fid(), &space, PxPoint { x: 1280, y: 0 }).unwrap_err();
        assert_eq!(
            err,
            MapError::OutOfFrame {
                frame: fid(),
                x: 1280,
                y: 0,
                image_w: 1280,
                image_h: 720,
            }
        );
    }

    #[test]
    fn uinput_normalizes_single_monitor() {
        let geo = DesktopGeometry::single(1920.0, 1080.0);
        let a = map_desktop_to_uinput(&geo, DesktopPoint { x: 0.0, y: 0.0 });
        assert_eq!(a, UInputAbs { x: 0, y: 0 });
        let a = map_desktop_to_uinput(&geo, DesktopPoint { x: 1920.0, y: 1080.0 });
        assert_eq!(a, UInputAbs { x: 65535, y: 65535 });
        let a = map_desktop_to_uinput(&geo, DesktopPoint { x: 960.0, y: 540.0 });
        assert_eq!(a.x, 32768); // 65535/2 rounds up
        assert_eq!(a.y, 32768);
    }

    #[test]
    fn uinput_offsets_negative_origin() {
        // Two 1920-wide monitors, left one at x = -1920.
        let geo = DesktopGeometry {
            min_x: -1920.0,
            min_y: 0.0,
            width: 3840.0,
            height: 1080.0,
        };
        // Primary monitor's top-left must map to the middle of the axis range.
        let a = map_desktop_to_uinput(&geo, DesktopPoint { x: 0.0, y: 0.0 });
        assert_eq!(a.x, 32768);
        assert_eq!(a.y, 0);
        // Far-left edge maps to 0.
        let a = map_desktop_to_uinput(&geo, DesktopPoint { x: -1920.0, y: 0.0 });
        assert_eq!(a.x, 0);
    }

    #[test]
    fn uinput_clamps_out_of_bounds() {
        let geo = DesktopGeometry::single(1920.0, 1080.0);
        let a = map_desktop_to_uinput(&geo, DesktopPoint { x: 99999.0, y: -5.0 });
        assert_eq!(a, UInputAbs { x: 65535, y: 0 });
    }

    #[test]
    fn end_to_end_left_monitor_fractional() {
        // Full story: 150%-scaled left monitor at x=-2560 (logical), captured
        // downscaled to 853x480. Model clicks image pixel (426, 240).
        let space = CoordSpace {
            image_w: 853,
            image_h: 480,
            origin_x: -2560.0,
            origin_y: 0.0,
            scale_x: 2560.0 / 853.0,
            scale_y: 1440.0 / 480.0,
        };
        let d = map_px_to_desktop(fid(), &space, PxPoint { x: 426, y: 240 }).unwrap();
        // 426 * (2560/853) ~= 1278.5 desktop px right of the monitor origin.
        assert!((d.x - (-1281.5)).abs() < 0.5, "d.x={}", d.x);
        assert!((d.y - 720.0).abs() < 0.5, "d.y={}", d.y);
        let geo = DesktopGeometry {
            min_x: -2560.0,
            min_y: 0.0,
            width: 2560.0 + 1920.0,
            height: 1440.0,
        };
        // The negative origin folds into the uinput axis: the center of the
        // left monitor (desktop x = -1280) sits at 1280/4480 of the range.
        let a = map_desktop_to_uinput(&geo, DesktopPoint { x: -1280.0, y: 720.0 });
        let expect_x = (1280.0f64 / 4480.0 * 65535.0).round() as u16;
        let expect_y = (720.0f64 / 1440.0 * 65535.0).round() as u16;
        assert_eq!(a.x, expect_x);
        assert_eq!(a.y, expect_y);
    }
}
