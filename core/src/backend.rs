//! Backend traits and mock implementations.
//!
//! The traits are the seam between the pure core (actions, frames, mapping)
//! and the real world (portal capture, uinput pointer, KWin scripting).
//! Mocks let the executor be tested without a live Plasma session — and
//! later serve as documentation of the exact call sequence the executor
//! expects from a real backend (see the module docs on each mock).

use crate::action::MouseButton;
use crate::coord::UInputAbs;
use crate::frame::CoordSpace;
use crate::result::ExecError;

/// One captured frame: raw image bytes plus the *measured* coordinate space.
///
/// The image bytes are opaque to the core (PNG, raw RGB — whatever the
/// capture backend produces); the `space` is what mapping runs on.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// Raw image bytes, handed to the model alongside the payload.
    pub bytes: Vec<u8>,
    /// Measured geometry of the captured image (downscale, scale, origin).
    pub space: CoordSpace,
}

/// Produces screenshots. One call = one frame.
pub trait CaptureBackend {
    fn screenshot(&mut self) -> Result<CapturedFrame, ExecError>;
}

/// Drives the pointer and keyboard.
///
/// Deliberately primitive: the *executor* owns sequencing and timing (the
/// Zetakai lesson — move→press settle, press hold, drag interpolation live
/// in the executor, not scattered across backends), so a backend only needs
/// to emit the raw events. Coordinates arrive as [`UInputAbs`]: the
/// executor has already folded in the frame's `CoordSpace` and the global
/// desktop geometry.
pub trait InputBackend {
    fn move_to(&mut self, p: UInputAbs) -> Result<(), ExecError>;
    fn press(&mut self, button: MouseButton) -> Result<(), ExecError>;
    fn release(&mut self, button: MouseButton) -> Result<(), ExecError>;
    /// Scroll ticks at the current pointer position (positive = right/down).
    fn wheel(&mut self, dx: f64, dy: f64) -> Result<(), ExecError>;
    /// Press-and-release a key combination, e.g. `["CTRL","L"]`.
    fn keypress(&mut self, keys: &[String]) -> Result<(), ExecError>;
    /// Insert literal Unicode text; never silently drop unlayoutable chars.
    fn type_text(&mut self, text: &str) -> Result<(), ExecError>;
    /// Release everything this backend could plausibly hold: every mouse
    /// button and every modifier key.
    ///
    /// The executor calls this as a last resort in its end-of-batch
    /// stuck-input sweep, when its own per-button `release()` failed. The
    /// default errors: a backend that can't enumerate what it holds must
    /// not claim a successful cleanup. Override with a real implementation
    /// — emitting release for an already-released key is a harmless no-op
    /// on evdev, so enumerate everything rather than track state.
    fn release_all(&mut self) -> Result<(), ExecError> {
        Err(ExecError::Backend(
            "release_all not implemented by this backend".into(),
        ))
    }
}

/// Opaque handle for a backend window. Distinct from [`FrameId`] by
/// construction: a window id is not a frame id and the type system says
/// so. `#[serde(transparent)]` keeps the wire shape a plain JSON number,
/// so the MCP schema and the debug log are byte-identical.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct WindowId(pub u64);

/// Describes one top-level window. `Serialize` so transports (MCP, debug
/// log) can hand the model the exact shape it acted on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub app_id: String,
    pub focused: bool,
    /// Frame-geometry bounds in desktop logical pixels, if the backend
    /// reports them. `None` means "not reported", not "zero-sized".
    pub bounds: Option<WindowBounds>,
}

/// Axis-aligned rectangle in desktop logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl WindowBounds {
    /// True when the point is inside the rectangle (top/left edges inclusive).
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// A filter for [`WindowBackend::find_window`]. All set criteria must match
/// (AND). An empty query matches every window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowQuery {
    /// Substring match against the window title (case-insensitive).
    pub title_substr: Option<String>,
    /// Exact match against the application id.
    pub app_id: Option<String>,
    /// Exact match against the backend window id.
    pub id: Option<WindowId>,
}

impl WindowQuery {
    pub fn matches(&self, w: &WindowInfo) -> bool {
        if let Some(id) = self.id {
            if w.id != id {
                return false;
            }
        }
        if let Some(app) = &self.app_id {
            if w.app_id != *app {
                return false;
            }
        }
        if let Some(sub) = &self.title_substr {
            if !w.title.to_lowercase().contains(&sub.to_lowercase()) {
                return false;
            }
        }
        true
    }
}

/// Window management (KWin scripting on the real backend), exposed to the
/// model through the `list_windows` / `active_window` / `focus_window` /
/// `window_bounds` actions.
pub trait WindowBackend {
    fn active_window(&mut self) -> Result<Option<WindowInfo>, ExecError>;
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>, ExecError>;
    /// Bring the window to the front / give it focus. Returns `false` when
    /// no window with that id exists; `true` means the request was issued
    /// (focus itself is asynchronous on the compositor — a subsequent
    /// `active_window` read is the confirmation).
    fn focus_window(&mut self, id: WindowId) -> Result<bool, ExecError>;
    /// Windows matching `query`, newest/most-recently-used first if the
    /// backend orders them that way. The default implementation filters
    /// `list_windows`; backends with a cheaper native query can override.
    fn find_window(&mut self, query: &WindowQuery) -> Result<Vec<WindowInfo>, ExecError> {
        Ok(self
            .list_windows()?
            .into_iter()
            .filter(|w| query.matches(w))
            .collect())
    }
    /// Frame-geometry bounds of one window, `None` when the window doesn't
    /// exist (or when the backend doesn't report bounds). The default
    /// implementation reads `list_windows`; backends with a cheaper
    /// single-window query can override.
    fn window_bounds(&mut self, id: WindowId) -> Result<Option<WindowBounds>, ExecError> {
        Ok(self
            .list_windows()?
            .into_iter()
            .find(|w| w.id == id)
            .and_then(|w| w.bounds))
    }
}

/// A fake capture backend with configurable, *known* geometry.
///
/// Returns synthetic image bytes (a header embedding the frame counter —
/// real pixels would be a lie here, the point is geometry) and the
/// configured [`CoordSpace`] on every call, so executor tests can assert the
/// full mapping chain against exact expectations.
#[derive(Debug)]
pub struct MockCapture {
    pub space: CoordSpace,
    /// How many screenshots have been taken; also tags the fake bytes.
    pub captures: u64,
    /// Set to fail the next capture with an infra error (retry testing).
    pub fail_next: Option<ExecError>,
}

impl MockCapture {
    pub fn new(space: CoordSpace) -> Self {
        Self {
            space,
            captures: 0,
            fail_next: None,
        }
    }
}

impl CaptureBackend for MockCapture {
    fn screenshot(&mut self) -> Result<CapturedFrame, ExecError> {
        if let Some(e) = self.fail_next.take() {
            return Err(e);
        }
        self.captures += 1;
        let bytes = format!(
            "PCUFAKE frame={} {}x{}",
            self.captures, self.space.image_w, self.space.image_h
        )
        .into_bytes();
        Ok(CapturedFrame {
            bytes,
            space: self.space,
        })
    }
}

/// One recorded input operation, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum InputOp {
    Move(UInputAbs),
    Press(MouseButton),
    Release(MouseButton),
    Wheel { dx: f64, dy: f64 },
    Keypress(Vec<String>),
    Type(String),
    /// [`InputBackend::release_all`] was called.
    ReleaseAll,
}

/// A recording input backend: every call appends to [`MockInput::log`].
///
/// The executor's unit tests assert against this log — it is the exact call
/// sequence a real backend must be able to emit, including timing-induced
/// ordering (move *then* settle *then* press).
#[derive(Debug, Default)]
pub struct MockInput {
    pub log: Vec<InputOp>,
    /// Fail the next input op with this error (retry-contract testing).
    pub fail_next: Option<ExecError>,
}

impl MockInput {
    pub fn new() -> Self {
        Self::default()
    }

    fn maybe_fail(&mut self) -> Result<(), ExecError> {
        if let Some(e) = self.fail_next.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl InputBackend for MockInput {
    fn move_to(&mut self, p: UInputAbs) -> Result<(), ExecError> {
        self.maybe_fail()?;
        self.log.push(InputOp::Move(p));
        Ok(())
    }

    fn press(&mut self, button: MouseButton) -> Result<(), ExecError> {
        self.maybe_fail()?;
        self.log.push(InputOp::Press(button));
        Ok(())
    }

    fn release(&mut self, button: MouseButton) -> Result<(), ExecError> {
        self.maybe_fail()?;
        self.log.push(InputOp::Release(button));
        Ok(())
    }

    fn wheel(&mut self, dx: f64, dy: f64) -> Result<(), ExecError> {
        self.maybe_fail()?;
        self.log.push(InputOp::Wheel { dx, dy });
        Ok(())
    }

    fn keypress(&mut self, keys: &[String]) -> Result<(), ExecError> {
        self.maybe_fail()?;
        self.log.push(InputOp::Keypress(keys.to_vec()));
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), ExecError> {
        self.maybe_fail()?;
        self.log.push(InputOp::Type(text.to_string()));
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), ExecError> {
        self.maybe_fail()?;
        self.log.push(InputOp::ReleaseAll);
        Ok(())
    }
}

/// A canned window list, driving the window actions in the schema.
#[derive(Debug, Default)]
pub struct MockWindow {
    pub windows: Vec<WindowInfo>,
}

impl MockWindow {
    pub fn new(windows: Vec<WindowInfo>) -> Self {
        Self { windows }
    }
}

impl WindowBackend for MockWindow {
    fn active_window(&mut self) -> Result<Option<WindowInfo>, ExecError> {
        Ok(self.windows.iter().find(|w| w.focused).cloned())
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>, ExecError> {
        Ok(self.windows.clone())
    }

    fn focus_window(&mut self, id: WindowId) -> Result<bool, ExecError> {
        if !self.windows.iter().any(|w| w.id == id) {
            return Ok(false);
        }
        for w in &mut self.windows {
            w.focused = w.id == id;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> CoordSpace {
        CoordSpace {
            image_w: 640,
            image_h: 480,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 2.0,
            scale_y: 2.0,
        }
    }

    #[test]
    fn mock_capture_returns_configured_space_and_counts() {
        let mut c = MockCapture::new(space());
        let a = c.screenshot().unwrap();
        let b = c.screenshot().unwrap();
        assert_eq!(a.space, space());
        assert_eq!(c.captures, 2);
        assert_ne!(a.bytes, b.bytes); // frame counter distinguishes them
    }

    #[test]
    fn mock_input_records_in_order() {
        let mut i = MockInput::new();
        let p = UInputAbs { x: 100, y: 200 };
        i.move_to(p).unwrap();
        i.press(MouseButton::Left).unwrap();
        i.release(MouseButton::Left).unwrap();
        assert_eq!(
            i.log,
            vec![
                InputOp::Move(p),
                InputOp::Press(MouseButton::Left),
                InputOp::Release(MouseButton::Left),
            ]
        );
    }

    #[test]
    fn mock_window_reports_focused() {
        let mut w = MockWindow::new(vec![
            WindowInfo {
                id: WindowId(1),
                title: "a".into(),
                app_id: "x".into(),
                focused: false,
                bounds: None,
            },
            WindowInfo {
                id: WindowId(2),
                title: "b".into(),
                app_id: "y".into(),
                focused: true,
                bounds: None,
            },
        ]);
        assert_eq!(w.active_window().unwrap().unwrap().id, WindowId(2));
        assert_eq!(w.list_windows().unwrap().len(), 2);
    }

    fn window_list() -> Vec<WindowInfo> {
        vec![
            WindowInfo {
                id: WindowId(1),
                title: "Konsole — root".into(),
                app_id: "org.kde.konsole".into(),
                focused: false,
                bounds: Some(WindowBounds {
                    x: 0.0,
                    y: 0.0,
                    w: 800.0,
                    h: 600.0,
                }),
            },
            WindowInfo {
                id: WindowId(2),
                title: "Mozilla Firefox".into(),
                app_id: "firefox".into(),
                focused: true,
                bounds: Some(WindowBounds {
                    x: 800.0,
                    y: 0.0,
                    w: 1120.0,
                    h: 900.0,
                }),
            },
        ]
    }

    #[test]
    fn focus_window_flips_focus_and_reports_unknown() {
        let mut w = MockWindow::new(window_list());
        assert!(w.focus_window(WindowId(1)).unwrap());
        assert_eq!(w.active_window().unwrap().unwrap().id, WindowId(1));
        assert!(!w.focus_window(WindowId(99)).unwrap());
        // unknown id leaves the current focus untouched
        assert_eq!(w.active_window().unwrap().unwrap().id, WindowId(1));
    }

    #[test]
    fn find_window_filters_by_criteria() {
        let mut w = MockWindow::new(window_list());
        let q = WindowQuery {
            title_substr: Some("firefox".into()),
            ..Default::default()
        };
        let hits = w.find_window(&q).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, WindowId(2));

        let q = WindowQuery {
            app_id: Some("org.kde.konsole".into()),
            ..Default::default()
        };
        assert_eq!(w.find_window(&q).unwrap().len(), 1);

        // criteria AND together; empty query matches everything
        let q = WindowQuery {
            title_substr: Some("console".into()),
            app_id: Some("firefox".into()),
            ..Default::default()
        };
        assert!(w.find_window(&q).unwrap().is_empty());
        assert_eq!(w.find_window(&WindowQuery::default()).unwrap().len(), 2);

        let q = WindowQuery {
            id: Some(WindowId(2)),
            ..Default::default()
        };
        assert_eq!(w.find_window(&q).unwrap().len(), 1);
    }

    #[test]
    fn window_bounds_reads_list_entry() {
        let mut w = MockWindow::new(window_list());
        let b = w.window_bounds(WindowId(2)).unwrap().unwrap();
        assert_eq!(b.w, 1120.0);
        assert!(b.contains(900.0, 100.0));
        assert!(!b.contains(799.9, 100.0));
        assert!(w.window_bounds(WindowId(99)).unwrap().is_none());
    }

    #[test]
    fn default_release_all_errors_honestly() {
        // A backend that doesn't override `release_all` must not claim a
        // successful cleanup: the sweep reports the button unreleased.
        struct NoReleaseAll;
        impl InputBackend for NoReleaseAll {
            fn move_to(&mut self, _: UInputAbs) -> Result<(), ExecError> {
                Ok(())
            }
            fn press(&mut self, _: MouseButton) -> Result<(), ExecError> {
                Ok(())
            }
            fn release(&mut self, _: MouseButton) -> Result<(), ExecError> {
                Ok(())
            }
            fn wheel(&mut self, _: f64, _: f64) -> Result<(), ExecError> {
                Ok(())
            }
            fn keypress(&mut self, _: &[String]) -> Result<(), ExecError> {
                Ok(())
            }
            fn type_text(&mut self, _: &str) -> Result<(), ExecError> {
                Ok(())
            }
        }
        let mut b = NoReleaseAll;
        assert!(matches!(b.release_all(), Err(ExecError::Backend(_))));
    }

    #[test]
    fn mock_release_all_logs_the_op() {
        let mut m = MockInput::new();
        m.release_all().unwrap();
        assert_eq!(m.log, vec![InputOp::ReleaseAll]);
    }
}
