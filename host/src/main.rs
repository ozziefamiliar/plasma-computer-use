//! `pcu-host`: the real-backend host binary for the pcu JSON-RPC stdio server.
//!
//! Same framing as the `pcu-stdio` demo binary, but wired to the real Plasma 6
//! backends from `pcu_backends`:
//!
//! - [`SpectacleCapture`] — fullscreen capture via `spectacle`, measured `CoordSpace`.
//! - [`UInputBackend`] — absolute uinput pointer + wheel + keyboard.
//! - [`KWinWindows`] — window listing via KWin scripting and D-Bus callbacks.
//! - [`RealClock`] — the executor's timing policy sleeps for real.
//!
//! Constructors probe their environment and fail with
//! [`ExecError::Infra`] naming the missing piece instead of failing obscurely
//! later; the host turns that into a plain-language startup diagnostic and
//! exits non-zero *before* serving any requests. Nothing here invents
//! geometry: the logical desktop bounding box comes from the same
//! `kscreen-doctor` layout the capture backend measures with.
//!
//! `--doctor` prints a JSON readiness report (the session-#1 "doctor-style
//! readiness JSON") and exits 0/1 without starting the server.

use pcu_backends::spectacle::{GeometryProbe, KScreenDoctor, LogicalMonitor, SpectacleCapture};
use pcu_backends::uinput::UInputBackend;
use pcu_backends::KWinWindows;
use pcu_core::backend::WindowBackend;
use pcu_core::frame::DesktopGeometry;
use pcu_core::result::ExecError;
use pcu_core::{Executor, RealClock, Timing};
use pcu_stdio::{serve, Server};
use std::io::{self, BufReader};

/// The logical desktop as a bounding box over the probed monitors.
///
/// Mirrors the capture backend's refusal: mixed per-output scales are not
/// guessed at, because `SpectacleCapture::measure_space` rejects them too —
/// a geometry that disagreed with the frame's own `CoordSpace` would poison
/// every coordinate the executor maps.
fn desktop_geometry(layout: &[LogicalMonitor]) -> Result<DesktopGeometry, ExecError> {
    if layout.is_empty() {
        return Err(ExecError::Backend("no usable outputs in layout".into()));
    }
    let scale = layout[0].scale;
    if layout.iter().any(|m| (m.scale - scale).abs() > f64::EPSILON) {
        let scales: Vec<f64> = layout.iter().map(|m| m.scale).collect();
        return Err(ExecError::Infra(format!(
            "mixed per-output scales not supported: {scales:?}"
        )));
    }
    let min_x = layout.iter().map(|m| m.x).min().unwrap_or(0) as f64;
    let min_y = layout.iter().map(|m| m.y).min().unwrap_or(0) as f64;
    let max_x = layout.iter().map(|m| m.x + m.width).max().unwrap_or(0) as f64;
    let max_y = layout.iter().map(|m| m.y + m.height).max().unwrap_or(0) as f64;
    Ok(DesktopGeometry {
        min_x,
        min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

/// One named readiness check: what was tried and what came back.
fn check(name: &str, outcome: Result<String, String>) -> serde_json::Value {
    match outcome {
        Ok(detail) => serde_json::json!({"name": name, "ok": true, "detail": detail}),
        Err(err) => serde_json::json!({"name": name, "ok": false, "error": err}),
    }
}

fn err_string(e: ExecError) -> String {
    e.to_string()
}

/// Probe everything the server needs and report as JSON.
/// Returns true when all checks passed.
fn doctor() -> bool {
    let checks = vec![
        check(
            "uinput",
            match UInputBackend::try_new() {
                Ok(_) => Ok("pointer+wheel+keyboard devices created".into()),
                Err(e) => Err(err_string(e)),
            },
        ),
        check(
            "geometry",
            match KScreenDoctor.probe() {
                Ok(layout) => match desktop_geometry(&layout) {
                    Ok(geo) => Ok(format!(
                        "{} monitor(s), desktop {:.0}x{:.0} at ({:.0},{:.0})",
                        layout.len(),
                        geo.width,
                        geo.height,
                        geo.min_x,
                        geo.min_y
                    )),
                    Err(e) => Err(err_string(e)),
                },
                Err(e) => Err(err_string(e)),
            },
        ),
        check(
            "spectacle",
            which("spectacle").ok_or_else(|| "spectacle not found on PATH".to_string()),
        ),
        check(
            "kwin",
            KWinWindows::try_new()
                .list_windows()
                .map(|windows| format!("script callback succeeded, {} window(s)", windows.len()))
                .map_err(err_string),
        ),
    ];
    let report = serde_json::json!({
        "ready": checks.iter().all(|c| c["ok"].as_bool().unwrap_or(false)),
        "checks": checks,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    report["ready"].as_bool().unwrap_or(false)
}

/// Minimal `which`: is `<bin>` runnable from PATH?
fn which(bin: &str) -> Option<String> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .ok()?;
    if out.status.success() {
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or(bin)
                .to_string(),
        )
    } else {
        None
    }
}

fn usage() {
    println!(
        "pcu-host: JSON-RPC 2.0 stdio server with real Plasma 6 backends.\n\n\
         Usage: pcu-host [--mime <type>] [--doctor]\n\n\
         Reads line-delimited JSON-RPC requests on stdin, writes responses on\n\
         stdout. --mime names the screenshot content type (default image/png,\n\
         since spectacle emits PNG). --doctor probes uinput, the desktop\n\
         geometry, spectacle and KWin window queries, prints a JSON readiness report, and\n\
         exits without serving."
    );
}

fn main() -> io::Result<()> {
    let mut mime = "image/png".to_string();
    let mut doctor_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mime" => {
                mime = args.next().unwrap_or_else(|| {
                    eprintln!("--mime requires a value");
                    std::process::exit(2);
                });
            }
            "--doctor" => doctor_only = true,
            "--help" | "-h" => {
                usage();
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    if doctor_only {
        std::process::exit(if doctor() { 0 } else { 1 });
    }

    // Fail fast with the probe's own error message: it already names the
    // missing piece (no /dev/uinput, no kscreen-doctor, mixed scales...).
    let input = UInputBackend::try_new().unwrap_or_else(|e| fatal("input backend", e));
    let layout = KScreenDoctor
        .probe()
        .unwrap_or_else(|e| fatal("desktop geometry", e));
    let geo = desktop_geometry(&layout).unwrap_or_else(|e| fatal("desktop geometry", e));
    let capture = SpectacleCapture::try_new();
    let windows = KWinWindows::try_new();

    eprintln!(
        "pcu-host: real backends up (desktop {:.0}x{:.0} at ({:.0},{:.0}))",
        geo.width, geo.height, geo.min_x, geo.min_y
    );

    let exec = Executor::new(
        capture,
        input,
        windows,
        RealClock,
        Timing::default(),
        geo,
    );
    let mut server = Server::new(exec, mime);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    serve(&mut server, BufReader::new(stdin.lock()), &mut stdout)
}

fn fatal(stage: &str, e: ExecError) -> ! {
    eprintln!("pcu-host: cannot start ({stage}): {e}");
    eprintln!("Run `pcu-host --doctor` for the full readiness report.");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: i32, h: i32, scale: f64) -> LogicalMonitor {
        LogicalMonitor {
            x,
            y,
            width: w,
            height: h,
            scale,
        }
    }

    #[test]
    fn single_monitor_identity() {
        let g = desktop_geometry(&[mon(0, 0, 1920, 1080, 1.0)]).unwrap();
        assert_eq!(
            g,
            DesktopGeometry {
                min_x: 0.0,
                min_y: 0.0,
                width: 1920.0,
                height: 1080.0
            }
        );
    }

    #[test]
    fn multi_monitor_negative_origins_bounding_box() {
        // Left monitor at scale 2.0: logical 960x540 at (-960, 0).
        let g = desktop_geometry(&[
            mon(-960, 0, 960, 540, 2.0),
            mon(0, 0, 1920, 1080, 2.0),
        ])
        .unwrap();
        assert_eq!(g.min_x, -960.0);
        assert_eq!(g.min_y, 0.0);
        assert_eq!(g.width, 2880.0);
        assert_eq!(g.height, 1080.0);
    }

    #[test]
    fn mixed_scales_refused_like_the_capture_backend() {
        let err = desktop_geometry(&[mon(0, 0, 1920, 1080, 1.0), mon(1920, 0, 1920, 1080, 1.25)]);
        assert!(err.is_err(), "mixed scales must refuse, not guess");
    }

    #[test]
    fn empty_layout_errors() {
        assert!(desktop_geometry(&[]).is_err());
    }
}
