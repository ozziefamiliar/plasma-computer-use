//! The batched action executor.
//!
//! The executor owns everything the model must not think about:
//!
//! - **Coordinate resolution.** Frame id → registry → `CoordSpace` →
//!   `DesktopPoint` → `UInputAbs`. Unknown or evicted frames and
//!   out-of-frame points are hard `Map` errors, never retried, and never
//!   abort the rest of the batch.
//! - **Timing.** The Zetakai-measured constants: 250ms move→press settle
//!   (30ms reliably *highlights* but never *activates*), 90ms press hold,
//!   450ms settle before a post-action screenshot, interpolated drags (~24
//!   steps, 12ms gaps — a single jump reads as a click). Timing is a policy
//!   here, not scattered across backends.
//! - **Recovery.** Only `Infra` errors are retried (bounded); `Map` and
//!   `Backend` errors go back to the model as failed outcomes. A failed
//!   action never aborts its batch, and a mid-batch `Screenshot` registers
//!   a frame that later actions in the *same* batch may reference.
//!
//! Sleeps go through [`Clock`] so tests record them without waiting.

use crate::action::{Action, Batch, MouseButton};
use crate::backend::{CaptureBackend, InputBackend, WindowBackend};
use crate::coord::{
    DesktopPoint, MapError, PxPoint, UInputAbs, map_desktop_to_uinput, map_px_to_desktop,
};
use crate::frame::{CoordSpace, DesktopGeometry, FrameId, FrameRegistry, ScreenshotDesc, MAX_FRAMES};
use crate::result::{ActionOutcome, BatchResult, ExecError};
use std::collections::VecDeque;
use std::time::Duration;

/// Where the executor sleeps. [`MockClock`] records; [`RealClock`] waits.
pub trait Clock {
    fn sleep(&self, d: Duration);
}

/// The real clock: actually sleeps.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealClock;

impl Clock for RealClock {
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A clock that records sleeps instead of taking them, so timing policy is
/// asserted by tests instead of felt by them.
#[derive(Debug, Default)]
pub struct MockClock {
    pub sleeps: std::cell::RefCell<Vec<Duration>>,
}

impl MockClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.borrow().clone()
    }
}

impl Clock for MockClock {
    fn sleep(&self, d: Duration) {
        self.sleeps.borrow_mut().push(d);
    }
}

/// Input timing policy. Defaults are the Zetakai measurements on real
/// hardware (COSMIC); override per-backend if measurement says otherwise.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Move→press settle. 30ms highlights; 250ms activates.
    pub move_settle: Duration,
    /// How long a press is held before release.
    pub press_hold: Duration,
    /// Settle before a post-action screenshot (post-action UI quiescence).
    pub screenshot_settle: Duration,
    /// Interpolated steps for a drag path (single jump = click).
    pub drag_steps: u32,
    /// Delay between drag interpolation steps.
    pub drag_step_delay: Duration,
    /// How many times an `Infra`-failed attempt is retried.
    pub max_infra_retries: u32,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            move_settle: Duration::from_millis(250),
            press_hold: Duration::from_millis(90),
            screenshot_settle: Duration::from_millis(450),
            drag_steps: 24,
            drag_step_delay: Duration::from_millis(12),
            max_infra_retries: 1,
        }
    }
}

/// Runs batches against concrete backends.
pub struct Executor<C, I, W, K> {
    capture: C,
    input: I,
    window: W,
    clock: K,
    timing: Timing,
    registry: FrameRegistry,
    geometry: DesktopGeometry,
    /// Screenshot bytes keyed by frame id, bounded like the registry.
    /// The registry holds geometry for mapping; a transport (MCP adapter,
    /// debug log) also needs the pixels the model must see. Real backends
    /// produce real bytes; the core treats them as opaque.
    screenshot_bytes: VecDeque<(FrameId, Vec<u8>)>,
}

impl<C, I, W, K> Executor<C, I, W, K>
where
    C: CaptureBackend,
    I: InputBackend,
    W: WindowBackend,
    K: Clock,
{
    pub fn new(
        capture: C,
        input: I,
        window: W,
        clock: K,
        timing: Timing,
        geometry: DesktopGeometry,
    ) -> Self {
        Self {
            capture,
            input,
            window,
            clock,
            timing,
            registry: FrameRegistry::new(),
            geometry,
            screenshot_bytes: VecDeque::new(),
        }
    }

    /// The full self-describing payload for a frame (MCP adapter's view).
    pub fn describe_frame(&self, id: FrameId) -> Option<ScreenshotDesc> {
        self.registry.lookup(id).map(|space| {
            crate::frame::FrameMeta { id, space: *space }.describe()
        })
    }

    /// The captured image bytes for a frame, if still retained.
    /// Transport-facing; mapping never touches this.
    pub fn screenshot_bytes(&self, id: FrameId) -> Option<&[u8]> {
        self.screenshot_bytes
            .iter()
            .find(|(fid, _)| *fid == id)
            .map(|(_, b)| b.as_slice())
    }

    pub fn input(&self) -> &I {
        &self.input
    }

    /// Mutable access to the window backend: capability probing and the
    /// (future) window actions run through here, not through the batch.
    pub fn window(&mut self) -> &mut W {
        &mut self.window
    }

    pub fn clock(&self) -> &K {
        &self.clock
    }

    pub fn registry_len(&self) -> usize {
        self.registry.len()
    }

    /// Execute a batch; outcomes are parallel to the input actions.
    pub fn execute(&mut self, batch: &Batch) -> BatchResult {
        let mut result = BatchResult::new();
        for action in &batch.0 {
            let (outcome, new_frame) = self.execute_one(action);
            if let Some(id) = new_frame {
                result.new_frames.push(id);
            }
            result.outcomes.push(outcome);
        }
        result
    }

    /// One action → (outcome, frame registered if it was a screenshot).
    fn execute_one(&mut self, action: &Action) -> (ActionOutcome, Option<FrameId>) {
        match action {
            Action::Screenshot { .. } => match self.take_screenshot() {
                Ok(id) => (ActionOutcome::Done { mapped: vec![] }, Some(id)),
                Err(e) => (ActionOutcome::Failed { error: e }, None),
            },
            Action::Move { frame, x, y } => self.coordinate_action(
                *frame,
                PxPoint { x: *x, y: *y },
                |input, clock, t, u| {
                    Self::retry(t, || {
                        input.move_to(u)?;
                        clock.sleep(t.move_settle);
                        Ok(())
                    })
                },
            ),
            Action::Click {
                frame,
                button,
                x,
                y,
            } => {
                let button = *button;
                self.coordinate_action(*frame, PxPoint { x: *x, y: *y }, |input, clock, t, u| {
                    Self::retry(t, || {
                        input.move_to(u)?;
                        clock.sleep(t.move_settle);
                        input.press(button)?;
                        clock.sleep(t.press_hold);
                        input.release(button)?;
                        Ok(())
                    })
                })
            }
            Action::DoubleClick {
                frame,
                button,
                x,
                y,
            } => {
                let button = *button;
                self.coordinate_action(*frame, PxPoint { x: *x, y: *y }, |input, clock, t, u| {
                    Self::retry(t, || {
                        for _ in 0..2 {
                            input.move_to(u)?;
                            clock.sleep(t.move_settle);
                            input.press(button)?;
                            clock.sleep(t.press_hold);
                            input.release(button)?;
                        }
                        Ok(())
                    })
                })
            }
            Action::Drag { frame, path } => self.drag(*frame, path),
            Action::Scroll {
                frame,
                x,
                y,
                dx,
                dy,
            } => {
                let (dx, dy) = (*dx, *dy);
                self.coordinate_action(*frame, PxPoint { x: *x, y: *y }, |input, clock, t, u| {
                    Self::retry(t, || {
                        input.move_to(u)?;
                        clock.sleep(t.move_settle);
                        input.wheel(dx, dy)?;
                        Ok(())
                    })
                })
            }
            Action::Keypress { keys } => {
                let t = self.timing;
                match Self::retry(t, || self.input.keypress(keys)) {
                    Ok(()) => (ActionOutcome::Done { mapped: vec![] }, None),
                    Err(e) => (ActionOutcome::Failed { error: e }, None),
                }
            }
            Action::Type { text } => {
                let t = self.timing;
                match Self::retry(t, || self.input.type_text(text)) {
                    Ok(()) => (ActionOutcome::Done { mapped: vec![] }, None),
                    Err(e) => (ActionOutcome::Failed { error: e }, None),
                }
            }
            Action::Wait { ms } => {
                self.clock.sleep(Duration::from_millis(*ms));
                (ActionOutcome::NoOp, None)
            }
        }
    }

    /// Shared shape for single-point coordinate actions: resolve the point
    /// against the frame registry first (pure `&self` borrow), then run the
    /// backend sequence with the resolved `UInputAbs`. Mapping failures
    /// become failed outcomes without ever touching the input backend.
    fn coordinate_action(
        &mut self,
        frame: FrameId,
        p: PxPoint,
        run: impl FnOnce(&mut I, &K, Timing, UInputAbs) -> Result<(), ExecError>,
    ) -> (ActionOutcome, Option<FrameId>) {
        match self.resolve(p, frame) {
            Ok((desktop, uinput)) => {
                let t = self.timing;
                match run(&mut self.input, &self.clock, t, uinput) {
                    Ok(()) => (ActionOutcome::Done { mapped: vec![desktop] }, None),
                    Err(e) => (ActionOutcome::Failed { error: e }, None),
                }
            }
            Err(e) => (ActionOutcome::Failed { error: e.into() }, None),
        }
    }

    fn drag(&mut self, frame: FrameId, path: &[PxPoint]) -> (ActionOutcome, Option<FrameId>) {
        // Resolve every point first: fail-fast on mapping, no partial press.
        let mut points = Vec::with_capacity(path.len());
        let mut mapped = Vec::with_capacity(path.len());
        for p in path {
            match self.resolve(*p, frame) {
                Ok((d, u)) => {
                    mapped.push(d);
                    points.push(u);
                }
                Err(e) => return (ActionOutcome::Failed { error: e.into() }, None),
            }
        }
        if points.is_empty() {
            return (
                ActionOutcome::Failed {
                    error: ExecError::Backend("drag needs at least one point".into()),
                },
                None,
            );
        }
        let t = self.timing;
        let steps = interpolate(&points, t.drag_steps);
        let outcome = Self::retry(t, || {
            self.input.move_to(points[0])?;
            self.clock.sleep(t.move_settle);
            self.input.press(MouseButton::Left)?;
            for p in &steps {
                self.input.move_to(*p)?;
                self.clock.sleep(t.drag_step_delay);
            }
            self.input.release(MouseButton::Left)?;
            Ok(())
        });
        match outcome {
            Ok(()) => (ActionOutcome::Done { mapped }, None),
            Err(e) => (ActionOutcome::Failed { error: e }, None),
        }
    }

    fn take_screenshot(&mut self) -> Result<FrameId, ExecError> {
        // Quiescence settle *before* capture, not after: the model must
        // see the state the previous action produced. It lives outside the
        // retry closure — a retry re-runs the capture, not the settle.
        self.clock.sleep(self.timing.screenshot_settle);
        let captured = Self::retry(self.timing, || self.capture.screenshot())?;
        let id = self.registry.register(captured.space);
        if self.screenshot_bytes.len() >= MAX_FRAMES {
            self.screenshot_bytes.pop_front();
        }
        self.screenshot_bytes.push_back((id, captured.bytes));
        Ok(id)
    }

    /// Resolve a screenshot-space point to (desktop, uinput) via the frame
    /// registry. Errors are `MapError`: never retried, never clamped.
    fn resolve(&self, p: PxPoint, frame: FrameId) -> Result<(DesktopPoint, UInputAbs), MapError> {
        let space: &CoordSpace = self
            .registry
            .lookup(frame)
            .ok_or(MapError::UnknownFrame(frame))?;
        let d = map_px_to_desktop(frame, space, p)?;
        let u = map_desktop_to_uinput(&self.geometry, d);
        Ok((d, u))
    }

    /// Retry only `Infra` errors, bounded. Everything else returns as-is.
    fn retry<T>(t: Timing, mut f: impl FnMut() -> Result<T, ExecError>) -> Result<T, ExecError> {
        let mut attempts = 0;
        loop {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) if e.retryable() && attempts < t.max_infra_retries => {
                    attempts += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Evenly spaced points along a polyline, `total` steps between the first
/// and last waypoint (endpoints included in the count budget, not repeated).
fn interpolate(path: &[UInputAbs], total: u32) -> Vec<UInputAbs> {
    if path.len() < 2 || total == 0 {
        return path.to_vec();
    }
    let segs: Vec<(f64, UInputAbs, UInputAbs)> = path
        .windows(2)
        .map(|w| {
            let (a, b) = (w[0], w[1]);
            let len = (((b.x as f64 - a.x as f64).powi(2)) + ((b.y as f64 - a.y as f64).powi(2))).sqrt();
            (len, a, b)
        })
        .collect();
    let total_len: f64 = segs.iter().map(|(l, _, _)| l).sum();
    let mut out = Vec::with_capacity(total as usize);
    if total_len == 0.0 {
        return vec![path[0]; total as usize];
    }
    let step_len = total_len / total as f64;
    let mut acc = 0.0;
    let mut seg_i = 0;
    let mut seg_start = 0.0;
    for _ in 1..=total {
        acc += step_len;
        while seg_i < segs.len() - 1 && acc > seg_start + segs[seg_i].0 {
            seg_start += segs[seg_i].0;
            seg_i += 1;
        }
        let (len, a, b) = segs[seg_i];
        let t = if len == 0.0 { 1.0 } else { ((acc - seg_start) / len).clamp(0.0, 1.0) };
        out.push(UInputAbs {
            x: (a.x as f64 + (b.x as f64 - a.x as f64) * t).round() as u16,
            y: (a.y as f64 + (b.y as f64 - a.y as f64) * t).round() as u16,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MockCapture, MockInput, MockWindow, InputOp};

    fn identity_space() -> CoordSpace {
        CoordSpace {
            image_w: 1920,
            image_h: 1080,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        }
    }

    fn executor() -> Executor<MockCapture, MockInput, MockWindow, MockClock> {
        Executor::new(
            MockCapture::new(identity_space()),
            MockInput::new(),
            MockWindow::default(),
            MockClock::new(),
            Timing::default(),
            DesktopGeometry::single(1920.0, 1080.0),
        )
    }

    fn screenshot_batch() -> Batch {
        Batch(vec![Action::Screenshot { note: None }])
    }

    #[test]
    fn timing_defaults_are_the_zetakai_measurements() {
        let t = Timing::default();
        assert_eq!(t.move_settle, Duration::from_millis(250));
        assert_eq!(t.press_hold, Duration::from_millis(90));
        assert_eq!(t.screenshot_settle, Duration::from_millis(450));
        assert_eq!((t.drag_steps, t.drag_step_delay), (24, Duration::from_millis(12)));
    }

    #[test]
    fn click_maps_coords_and_applies_timing() {
        let mut ex = executor();
        let r = ex.execute(&screenshot_batch());
        assert!(r.all_ok());
        let frame = r.new_frames[0];

        let batch = Batch(vec![Action::Click {
            frame,
            button: MouseButton::Left,
            x: 960,
            y: 540,
        }]);
        let r = ex.execute(&batch);
        assert!(r.all_ok());
        match &r.outcomes[0] {
            ActionOutcome::Done { mapped } => {
                assert_eq!(mapped.len(), 1);
                assert!((mapped[0].x - 960.0).abs() < 1e-9);
            }
            o => panic!("expected Done, got {:?}", o),
        }
        // 960/1920 * 65535 = 32767.5 → 32768 (halves away from zero).
        let expected = UInputAbs { x: 32768, y: 32768 };
        assert_eq!(
            ex.input().log,
            vec![
                InputOp::Move(expected),
                InputOp::Press(MouseButton::Left),
                InputOp::Release(MouseButton::Left),
            ]
        );
        // Timing: 450ms settle happened at screenshot time; click adds
        // 250ms move settle + 90ms press hold.
        assert_eq!(
            ex.clock().sleeps(),
            vec![
                Duration::from_millis(450),
                Duration::from_millis(250),
                Duration::from_millis(90),
            ]
        );
    }

    #[test]
    fn screenshot_mid_batch_registers_frame_for_later_actions() {
        let mut ex = executor();
        let batch = Batch(vec![
            Action::Screenshot { note: None },
            Action::Move {
                frame: FrameId(1),
                x: 0,
                y: 0,
            },
        ]);
        let r = ex.execute(&batch);
        assert!(r.all_ok());
        assert_eq!(r.new_frames, vec![FrameId(1)]);
        assert_eq!(ex.registry_len(), 1);
    }

    #[test]
    fn unknown_frame_fails_without_touching_input_and_batch_continues() {
        let mut ex = executor();
        let batch = Batch(vec![
            Action::Click {
                frame: FrameId(99),
                button: MouseButton::Left,
                x: 10,
                y: 10,
            },
            Action::Wait { ms: 5 },
        ]);
        let r = ex.execute(&batch);
        assert!(!r.all_ok());
        assert!(ex.input().log.is_empty()); // no invented clicks
        match &r.outcomes[0] {
            ActionOutcome::Failed {
                error: ExecError::Map(MapError::UnknownFrame(FrameId(99))),
            } => {}
            o => panic!("expected UnknownFrame failure, got {:?}", o),
        }
        assert_eq!(r.outcomes[1], ActionOutcome::NoOp); // batch continued
    }

    #[test]
    fn capture_retry_does_not_resettle() {
        let mut ex = executor();
        // First capture fails with infra; retry must re-run the capture
        // but not re-sleep the 450ms pre-capture settle.
        ex.capture.fail_next = Some(ExecError::Infra("portal hiccup".into()));
        let r = ex.execute(&screenshot_batch());
        assert!(r.all_ok());
        assert_eq!(ex.capture.captures, 1);
        assert_eq!(ex.clock().sleeps(), vec![Duration::from_millis(450)]);
    }

    #[test]
    fn out_of_frame_errors_are_not_clamped() {
        let mut ex = executor();
        ex.execute(&screenshot_batch());
        let batch = Batch(vec![Action::Click {
            frame: FrameId(1),
            button: MouseButton::Left,
            x: 1920, // image is 1920 wide: 0..=1919 valid
            y: 10,
        }]);
        let r = ex.execute(&batch);
        match &r.outcomes[0] {
            ActionOutcome::Failed {
                error: ExecError::Map(MapError::OutOfFrame { .. }),
            } => {}
            o => panic!("expected OutOfFrame failure, got {:?}", o),
        }
        assert!(ex.input().log.is_empty());
    }

    #[test]
    fn infra_failure_retries_once_then_succeeds() {
        let mut ex = executor();
        ex.execute(&screenshot_batch());
        ex.input.fail_next = Some(ExecError::Infra("portal hiccup".into()));
        let r = ex.execute(&Batch(vec![Action::Click {
            frame: FrameId(1),
            button: MouseButton::Left,
            x: 10,
            y: 10,
        }]));
        assert!(r.all_ok());
        // First attempt's move failed (not logged), retry ran the full
        // sequence: exactly one successful move/press/release.
        let moves = ex
            .input()
            .log
            .iter()
            .filter(|op| matches!(op, InputOp::Move(_)))
            .count();
        assert_eq!(moves, 1);
        assert_eq!(ex.input().log.len(), 3);
    }

    #[test]
    fn backend_error_is_not_retried() {
        let mut ex = executor();
        ex.execute(&screenshot_batch());
        ex.input.fail_next = Some(ExecError::Backend("unresolvable key".into()));
        let r = ex.execute(&Batch(vec![Action::Keypress {
            keys: vec!["CTRL".into()],
        }]));
        assert!(!r.all_ok());
        assert!(ex.input().log.is_empty()); // attempted once, failed, no retry
    }

    #[test]
    fn drag_interpolates_steps_between_press_and_release() {
        let mut ex = executor();
        ex.execute(&screenshot_batch());
        let path = vec![
            PxPoint { x: 0, y: 0 },
            PxPoint { x: 100, y: 0 },
        ];
        let r = ex.execute(&Batch(vec![Action::Drag {
            frame: FrameId(1),
            path,
        }]));
        assert!(r.all_ok());
        let log = &ex.input().log;
        assert_eq!(log[0], InputOp::Move(UInputAbs { x: 0, y: 0 }));
        assert_eq!(log[1], InputOp::Press(MouseButton::Left));
        assert_eq!(log[log.len() - 1], InputOp::Release(MouseButton::Left));
        let moves: Vec<_> = log
            .iter()
            .filter_map(|op| match op {
                InputOp::Move(p) => Some(*p),
                _ => None,
            })
            .collect();
        assert_eq!(moves.len(), 1 + 24); // initial move + 24 interpolation steps
        // Interpolation starts one step along the path, not repeating the
        // start point (the initial move_to already positioned there).
        assert!(moves[1].x > 0 && moves[1].x < 300);
        // Steps advance monotonically toward the path end.
        assert!(moves.windows(2).skip(1).all(|w| w[0].x <= w[1].x));
        // Last interpolated step lands at (or adjacent to) the path end.
        let last = moves[moves.len() - 1];
        assert!((last.x as i32 - 3413).abs() <= 1); // 100/1920*65535 = 3413.3
        assert_eq!(last.y, 0);
        // Drag steps sleep 12ms each.
        let step_sleeps = ex
            .clock()
            .sleeps()
            .iter()
            .filter(|d| **d == Duration::from_millis(12))
            .count();
        assert_eq!(step_sleeps, 24);
    }

    #[test]
    fn scroll_and_type_record_ops() {
        let mut ex = executor();
        ex.execute(&screenshot_batch());
        let r = ex.execute(&Batch(vec![
            Action::Scroll {
                frame: FrameId(1),
                x: 10,
                y: 10,
                dx: 0.0,
                dy: 3.0,
            },
            Action::Type {
                text: "héllo 🐺".into(),
            },
        ]));
        assert!(r.all_ok());
        assert!(ex.input().log.contains(&InputOp::Wheel { dx: 0.0, dy: 3.0 }));
        assert!(ex
            .input()
            .log
            .contains(&InputOp::Type("héllo 🐺".into())));
    }
}
