//! Fullscreen capture via the `spectacle` CLI.
//!
//! The repo survey (session #1) settled this: zetakai's `spectacle`
//! subprocess is the proven KDE route (`libwayshot` is a dead end on KWin,
//! and neither repo implements the portal PipeWire path). The exact argv is
//! zetakai's: `spectacle -b -n -f -o <path>` (background, no-notify,
//! fullscreen, output file).
//!
//! The interesting half is the *measured* [`CoordSpace`][pcu_core::frame::CoordSpace]:
//! - `image_w`/`image_h` come from the PNG's own IHDR chunk (parsed here,
//!   pure std — no image crate for 8 bytes of header).
//! - the logical desktop rect and per-output scales come from
//!   [`KScreenDoctor`] (`kscreen-doctor -j`; JSON shape and the
//!   connected/enabled/non-mirrored filter follow agent-sh's
//!   `parse_kscreen_monitor_layout`, including `replicationSource`).
//! - `scale = image / logical`. Mixed per-output scales are refused with a
//!   clear error (per-output capture is future work — guessing one scale
//!   would be inventing geometry), and if the image dimensions don't match
//!   `logical * scale` within a pixel, that's an error too, not a silent
//!   rescale.
//!
//! LIVE-VALIDATION (Arch machine):
//! - what pixel space does spectacle's fullscreen capture actually produce
//!   on mixed-DPI Plasma (device pixels of the virtual desktop? per-output
//!   max scale?)? The ±1px correspondence check will confirm or deny;
//! - `kscreen-doctor -j`'s `pos` is treated as *logical* coordinates and
//!   `size` as *physical* (per agent-sh) — verify against a real session.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use pcu_core::backend::{CaptureBackend, CapturedFrame};
use pcu_core::frame::CoordSpace;
use pcu_core::result::ExecError;

/// One output in logical coordinates: position plus logical size plus the
/// scale that produced the logical size from the physical mode.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalMonitor {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
}

/// Probes the desktop geometry. Swappable so tests can feed canned layouts.
pub trait GeometryProbe {
    fn probe(&self) -> Result<Vec<LogicalMonitor>, ExecError>;
}

// ---- kscreen-doctor -j parsing (agent-sh's recipe) ----------------------------

#[derive(serde::Deserialize)]
struct KscreenConfig {
    outputs: Vec<KscreenOutput>,
}

#[derive(serde::Deserialize)]
struct KscreenOutput {
    pos: KscreenPoint,
    size: KscreenSize,
    scale: f64,
    connected: bool,
    enabled: bool,
    #[serde(default, rename = "replicationSource")]
    replication_source: Option<i64>,
}

#[derive(serde::Deserialize)]
struct KscreenPoint {
    x: i32,
    y: i32,
}

#[derive(serde::Deserialize)]
struct KscreenSize {
    width: i32,
    height: i32,
}

/// Parse `kscreen-doctor -j` stdout into logical monitors.
///
/// Mirrors agent-sh's `parse_kscreen_monitor_layout`: keep connected +
/// enabled + non-mirrored outputs (`replicationSource == 0` means mirrored
/// onto output 0); logical size = physical / scale, rounded.
pub fn parse_kscreen_layout(json: &[u8]) -> Option<Vec<LogicalMonitor>> {
    let config: KscreenConfig = serde_json::from_slice(json).ok()?;
    let layout = config
        .outputs
        .into_iter()
        .filter(|o| o.connected && o.enabled && o.replication_source.unwrap_or(0) == 0)
        .map(|o| {
            if !o.scale.is_finite() || o.scale <= 0.0 || o.size.width <= 0 || o.size.height <= 0 {
                return None;
            }
            let width = (f64::from(o.size.width) / o.scale).round() as i32;
            let height = (f64::from(o.size.height) / o.scale).round() as i32;
            (width > 0 && height > 0).then_some(LogicalMonitor {
                x: o.pos.x,
                y: o.pos.y,
                width,
                height,
                scale: o.scale,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!layout.is_empty()).then_some(layout)
}

/// Geometry probe via `kscreen-doctor -j`.
pub struct KScreenDoctor;

impl GeometryProbe for KScreenDoctor {
    fn probe(&self) -> Result<Vec<LogicalMonitor>, ExecError> {
        let out = Command::new("kscreen-doctor")
            .arg("-j")
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ExecError::Infra(
                        "kscreen-doctor not found; cannot measure the desktop geometry".into(),
                    )
                } else {
                    ExecError::Infra(format!("failed to run kscreen-doctor: {e}"))
                }
            })?;
        if !out.status.success() {
            return Err(ExecError::Infra(format!(
                "kscreen-doctor -j failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        parse_kscreen_layout(&out.stdout).ok_or_else(|| {
            ExecError::Backend("kscreen-doctor -j output unparseable or no usable outputs".into())
        })
    }
}

// ---- PNG dimensions (IHDR only) -------------------------------------------------

/// Read (width, height) from a PNG's IHDR chunk. The full image decode is
/// the model's business; the backend only needs the dimensions for the
/// coordinate space.
pub fn png_dimensions(png: &[u8]) -> Result<(u32, u32), ExecError> {
    const SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    let bad = || ExecError::Backend("capture is not a PNG (bad header)".into());
    if png.len() < 24 || &png[0..8] != SIG {
        return Err(bad());
    }
    let len = u32::from_be_bytes(png[8..12].try_into().unwrap());
    if &png[12..16] != b"IHDR" || len < 13 {
        return Err(bad());
    }
    let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
    if w == 0 || h == 0 {
        return Err(bad());
    }
    Ok((w, h))
}

// ---- the backend ------------------------------------------------------------------

static SHOT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Fullscreen capture via `spectacle`, with a measured [`CoordSpace`].
pub struct SpectacleCapture<P: GeometryProbe = KScreenDoctor> {
    probe: P,
}

impl SpectacleCapture<KScreenDoctor> {
    pub fn try_new() -> Self {
        Self {
            probe: KScreenDoctor,
        }
    }
}

impl<P: GeometryProbe> SpectacleCapture<P> {
    pub fn with_probe(probe: P) -> Self {
        Self { probe }
    }

    /// Build the measured coordinate space from the image dimensions and
    /// the probed layout. Pure function — unit-testable without hardware.
    fn measure_space(
        image_w: u32,
        image_h: u32,
        layout: &[LogicalMonitor],
    ) -> Result<CoordSpace, ExecError> {
        if layout.is_empty() {
            return Err(ExecError::Backend("no usable outputs in layout".into()));
        }
        // Mixed per-output scales: refuse to guess. Per-output capture
        // (or the portal ScreenCast path) is the real fix.
        let scale = layout[0].scale;
        if layout.iter().any(|m| (m.scale - scale).abs() > f64::EPSILON) {
            let scales: Vec<f64> = layout.iter().map(|m| m.scale).collect();
            return Err(ExecError::Backend(format!(
                "mixed per-output scales {scales:?}: fullscreen capture has no \
                 single coordinate space; per-output capture not yet implemented"
            )));
        }
        let min_x = layout.iter().map(|m| m.x).min().unwrap();
        let min_y = layout.iter().map(|m| m.y).min().unwrap();
        let max_x = layout.iter().map(|m| m.x + m.width).max().unwrap();
        let max_y = layout.iter().map(|m| m.y + m.height).max().unwrap();
        let logical_w = (max_x - min_x) as f64;
        let logical_h = (max_y - min_y) as f64;

        // The image must correspond to logical*scale within a pixel —
        // rounding across outputs can shift an edge by one. Anything more
        // means our model of what spectacle captured is wrong, and mapping
        // against it would invent clicks.
        let expect_w = logical_w * scale;
        let expect_h = logical_h * scale;
        if (image_w as f64 - expect_w).abs() > 1.0 || (image_h as f64 - expect_h).abs() > 1.0 {
            return Err(ExecError::Backend(format!(
                "capture is {image_w}x{image_h}px but probed geometry is \
                 {logical_w}x{logical_h} logical @ {scale}x (expected \
                 {expect_w:.0}x{expect_h:.0}px): cannot build a trustworthy \
                 coordinate space"
            )));
        }

        Ok(CoordSpace {
            image_w,
            image_h,
            origin_x: min_x as f64,
            origin_y: min_y as f64,
            scale_x: image_w as f64 / logical_w,
            scale_y: image_h as f64 / logical_h,
        })
    }

    fn shot_path() -> PathBuf {
        let n = SHOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pcu-shot-{}-{n}.png", std::process::id()))
    }
}

impl<P: GeometryProbe> CaptureBackend for SpectacleCapture<P> {
    fn screenshot(&mut self) -> Result<CapturedFrame, ExecError> {
        let path = Self::shot_path();
        // Exact zetakai argv: background, no-notify, fullscreen, output file.
        let out = Command::new("spectacle")
            .args(["-b", "-n", "-f", "-o"])
            .arg(&path)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ExecError::Infra("spectacle not found; is this a Plasma session?".into())
                } else {
                    ExecError::Infra(format!("failed to run spectacle: {e}"))
                }
            })?;
        if !out.status.success() {
            return Err(ExecError::Infra(format!(
                "spectacle failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let bytes = std::fs::read(&path).map_err(|e| {
            ExecError::Infra(format!("spectacle produced no output file: {e}"))
        })?;
        let _ = std::fs::remove_file(&path); // best-effort cleanup
        let (image_w, image_h) = png_dimensions(&bytes)?;
        let layout = self.probe.probe()?;
        let space = Self::measure_space(image_w, image_h, &layout)?;
        Ok(CapturedFrame { bytes, space })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal synthetic PNG: signature + IHDR(w,h) + IEND. Enough for the
    /// header parser; not a viewable image.
    fn fake_png(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit truecolor
        v.extend_from_slice(&0u32.to_be_bytes()); // crc (unchecked)
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(b"IEND");
        v.extend_from_slice(&0u32.to_be_bytes());
        v
    }

    #[test]
    fn png_dimensions_reads_ihdr() {
        assert_eq!(png_dimensions(&fake_png(1920, 1080)).unwrap(), (1920, 1080));
        assert_eq!(png_dimensions(&fake_png(1, 1)).unwrap(), (1, 1));
    }

    #[test]
    fn png_dimensions_rejects_garbage() {
        assert!(png_dimensions(b"not a png").is_err());
        assert!(png_dimensions(&fake_png(0, 1080)).is_err());
        let mut bad = fake_png(100, 100);
        bad[12..16].copy_from_slice(b"XXXX");
        assert!(png_dimensions(&bad).is_err());
    }

    #[test]
    fn kscreen_layout_parses_agent_sh_fixture() {
        // Fixture straight from agent-sh's own test (physical sizes, scales).
        let json = br#"{"outputs":[
            {"pos":{"x":0,"y":0},"size":{"width":3840,"height":2160},"scale":2.0,"connected":true,"enabled":true},
            {"pos":{"x":1920,"y":0},"size":{"width":1440,"height":2560},"scale":1.0,"connected":true,"enabled":true},
            {"pos":{"x":0,"y":0},"size":{"width":0,"height":0},"scale":0.0,"connected":false,"enabled":false}
        ]}"#;
        let layout = parse_kscreen_layout(json).unwrap();
        assert_eq!(layout.len(), 2);
        assert_eq!(
            layout[0],
            LogicalMonitor { x: 0, y: 0, width: 1920, height: 1080, scale: 2.0 }
        );
        assert_eq!(
            layout[1],
            LogicalMonitor { x: 1920, y: 0, width: 1440, height: 2560, scale: 1.0 }
        );
    }

    #[test]
    fn kscreen_layout_excludes_mirrored() {
        let json = br#"{"outputs":[
            {"pos":{"x":0,"y":0},"size":{"width":1920,"height":1080},"scale":1.0,"connected":true,"enabled":true,"replicationSource":0},
            {"pos":{"x":1920,"y":0},"size":{"width":3840,"height":2160},"scale":2.0,"connected":true,"enabled":true,"replicationSource":1}
        ]}"#;
        let layout = parse_kscreen_layout(json).unwrap();
        assert_eq!(layout.len(), 1);
    }

    #[test]
    fn measure_space_single_output_2x() {
        let layout = vec![LogicalMonitor { x: 0, y: 0, width: 1920, height: 1080, scale: 2.0 }];
        let space = SpectacleCapture::<KScreenDoctor>::measure_space(3840, 2160, &layout).unwrap();
        assert_eq!((space.image_w, space.image_h), (3840, 2160));
        assert_eq!((space.origin_x, space.origin_y), (0.0, 0.0));
        assert!((space.scale_x - 2.0).abs() < 1e-9);
        assert!((space.scale_y - 2.0).abs() < 1e-9);
    }

    #[test]
    fn measure_space_negative_origin_multi_monitor() {
        // Left monitor at x=-1920 (core's negative-origin case), uniform 1x.
        let layout = vec![
            LogicalMonitor { x: -1920, y: 0, width: 1920, height: 1080, scale: 1.0 },
            LogicalMonitor { x: 0, y: 0, width: 1920, height: 1080, scale: 1.0 },
        ];
        let space = SpectacleCapture::<KScreenDoctor>::measure_space(3840, 1080, &layout).unwrap();
        assert_eq!((space.origin_x, space.origin_y), (-1920.0, 0.0));
        assert!((space.scale_x - 1.0).abs() < 1e-9);
    }

    #[test]
    fn measure_space_refuses_mixed_scales() {
        let layout = vec![
            LogicalMonitor { x: 0, y: 0, width: 1920, height: 1080, scale: 2.0 },
            LogicalMonitor { x: 1920, y: 0, width: 1440, height: 2560, scale: 1.0 },
        ];
        let err = SpectacleCapture::<KScreenDoctor>::measure_space(3360, 3640, &layout)
            .unwrap_err();
        assert!(matches!(err, ExecError::Backend(_)));
        assert!(err.to_string().contains("mixed per-output scales"));
    }

    #[test]
    fn measure_space_refuses_dimension_mismatch() {
        // Probed 1920x1080@2x but the image is 1920x1080: something's off.
        let layout = vec![LogicalMonitor { x: 0, y: 0, width: 1920, height: 1080, scale: 2.0 }];
        let err = SpectacleCapture::<KScreenDoctor>::measure_space(1920, 1080, &layout)
            .unwrap_err();
        assert!(err.to_string().contains("cannot build a trustworthy coordinate space"));
    }

    #[test]
    fn measure_space_tolerates_one_px_rounding() {
        let layout = vec![LogicalMonitor { x: 0, y: 0, width: 1920, height: 1080, scale: 1.0 }];
        // Off by exactly one pixel: allowed (edge rounding across outputs).
        assert!(SpectacleCapture::<KScreenDoctor>::measure_space(1921, 1080, &layout).is_ok());
        // Off by two: not.
        assert!(SpectacleCapture::<KScreenDoctor>::measure_space(1922, 1080, &layout).is_err());
    }
}
