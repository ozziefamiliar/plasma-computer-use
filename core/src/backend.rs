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
}

/// Describes one top-level window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub focused: bool,
}

/// Window management (KWin scripting on the real backend).
///
/// Not yet exercised by the action schema — no window actions exist — but the
/// seam is here so the real KDE backend can implement it alongside capture
/// and input, and so capability probing has a shape to probe.
pub trait WindowBackend {
    fn active_window(&mut self) -> Result<Option<WindowInfo>, ExecError>;
    fn list_windows(&mut self) -> Result<Vec<WindowInfo>, ExecError>;
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
}

/// A canned window list. Not exercised by the executor yet (no window
/// actions in the schema); present so the backend seam is complete and
/// probed in one place.
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
                id: 1,
                title: "a".into(),
                app_id: "x".into(),
                focused: false,
            },
            WindowInfo {
                id: 2,
                title: "b".into(),
                app_id: "y".into(),
                focused: true,
            },
        ]);
        assert_eq!(w.active_window().unwrap().unwrap().id, 2);
        assert_eq!(w.list_windows().unwrap().len(), 2);
    }
}
