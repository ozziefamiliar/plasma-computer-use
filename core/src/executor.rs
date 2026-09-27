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
use crate::backend::{CaptureBackend, InputBackend, WindowBackend, WindowQuery};
use crate::coord::{
    DesktopPoint, MapError, PxPoint, UInputAbs, map_desktop_to_uinput, map_px_to_desktop,
};
use crate::frame::{CoordSpace, DesktopGeometry, FrameId, FrameRegistry, ScreenshotDesc, MAX_FRAMES};
use crate::result::{ActionOutcome, BatchResult, ExecError};
use std::collections::{HashSet, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Where the executor sleeps. [`MockClock`] records; [`RealClock`] waits.
pub trait Clock {
    fn sleep(&self, d: Duration);

    /// The current wall-clock time. Used by the rate limiter's token bucket.
    /// The default is real time; test clocks override it with a manual time.
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The fingerprint a confirmation grant is keyed on: FNV-1a over the
/// action's `Debug` form. Debug is deterministic within a build and covers
/// every field, so a grant authorizes *exactly* the approved action —
/// confirming `type "rm -rf /tmp/cache"` never authorizes
/// `type "rm -rf /"`. Grants are in-process and single-use; they are not
/// serialized or persisted.
fn confirmation_key(action: &Action) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in format!("{:?}", action).bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
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

/// A thread-safe handle for emergency cancellation.
///
/// Cloned freely and handed to whoever needs to stop the executor: the
/// stdio router's `notifications/cancelled` handler, a signal handler, a
/// watchdog thread. The flag takes effect at the next *action boundary* —
/// the currently executing action (e.g. a 24-step drag) runs to completion,
/// then every remaining action in the batch drains as
/// [`ExecError::Cancelled`] without touching backends. The flag is
/// one-shot: the first `execute()` that observes it clears it, so a stale
/// cancel can never poison a later batch.
///
/// Mid-*batch* cancellation over the current single-threaded stdio loop is
/// limited by the transport: `execute()` blocks, so a cancel line can't be
/// read until the batch finishes. The flag takes effect at the first
/// action boundary of the *next* batch. A host that runs `execute()` on a
/// worker thread gets true mid-batch cancellation for free.
#[derive(Debug, Clone)]
pub struct CancelHandle {
    flag: Arc<AtomicBool>,
}

impl CancelHandle {
    /// Arm cancellation: the next action boundary drains the batch.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Disarm without executing: drop a stale cancel.
    pub fn clear(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Whether cancellation is currently armed.
    pub fn is_armed(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
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
    /// Safety policy applied to every batch before execution.
    policy: crate::guard::Policy,
    /// Action-rate limiter (token bucket). Checked at every action boundary
    /// for actions that pass the guard review; `None` disables. On by
    /// default — the spec's safety minimum.
    rate_limit: Option<crate::rate::RateLimiter>,
    /// Emergency-cancel flag; shared by clone with whoever holds a
    /// [`CancelHandle`].
    cancel: Arc<AtomicBool>,
    /// Mouse buttons the executor pressed but hasn't released yet. Only
    /// ever non-empty when a release failed: [`Self::tracked_release`]
    /// records the press, and [`Self::sweep_stuck`] drains the set at
    /// every batch boundary.
    held: Vec<MouseButton>,
    /// One-shot operator approvals for `NeedsConfirm` actions, keyed by
    /// [`confirmation_key`]. A grant is consumed the first time its exact
    /// action content reaches a `NeedsConfirm` verdict; unexpired grants
    /// are dropped by [`Executor::clear_confirmations`].
    grants: HashSet<u64>,
    /// Optional operator prompt, consulted at most once per `execute()` —
    /// at the first `NeedsConfirm` action that has no grant. Receives the
    /// batch and the full `(index, reason)` pending list, returns the
    /// indices the operator approved; those become grants. The host owns
    /// the UX (a TTY prompt, a chat message, anything); core only defines
    /// the seam. `None` (the default) means unconfirmed actions fail in
    /// place, exactly as before the channel existed.
    confirm_hook: Option<Box<dyn Fn(&Batch, &[(usize, String)]) -> Vec<usize> + Send>>,
    /// Whether the confirm hook has been consulted in the current
    /// `execute()` call. Reset per batch so a hook is never asked twice
    /// for one batch.
    hook_consulted: bool,
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
        // Buckets start full; construction time is the clock's now. Read the
        // time before `clock` moves into the struct.
        let limiter = crate::rate::RateLimiter::new(crate::rate::RateLimit::default(), clock.now());
        Self {
            capture,
            input,
            window,
            clock,
            timing,
            registry: FrameRegistry::new(),
            geometry,
            screenshot_bytes: VecDeque::new(),
            policy: crate::guard::Policy::default(),
            rate_limit: Some(limiter),
            cancel: Arc::new(AtomicBool::new(false)),
            held: Vec::new(),
            grants: HashSet::new(),
            confirm_hook: None,
            hook_consulted: false,
        }
    }

    /// Replace the safety policy (hosts load it from config).
    pub fn set_policy(&mut self, policy: crate::guard::Policy) {
        self.policy = policy;
    }

    /// Flip read-only mode without rebuilding the policy: only `Screenshot`
    /// actions run; every other action fails in place with a guard error.
    /// Convenient for hosts wiring a `--read-only` flag.
    pub fn set_read_only(&mut self, on: bool) {
        self.policy.read_only = on;
    }

    /// Replace the action-rate limiter (`None` disables it; the limiter is
    /// on by default with the safety-minimum config). Reconfiguring starts
    /// the bucket full, so a freshly loosened limit applies immediately.
    pub fn set_rate_limit(&mut self, limit: Option<crate::rate::RateLimit>) {
        self.rate_limit = limit.map(|l| crate::rate::RateLimiter::new(l, self.clock.now()));
    }

    /// Grant one-shot confirmation for `action`: the next `execute()` that
    /// reaches a `NeedsConfirm` verdict on byte-identical action content
    /// runs it instead of failing. The grant is consumed on first use, so
    /// the same grant never authorizes a second execution, and it is keyed
    /// on exact content — approving `type "rm -rf /tmp/cache"` does not
    /// approve `type "rm -rf /"`. Use this when the operator's approval
    /// arrives out-of-band (a chat message, an API call); for an
    /// interactive prompt during execution see [`Executor::set_confirm_hook`].
    pub fn grant_confirmation(&mut self, action: &Action) {
        self.grants.insert(confirmation_key(action));
    }

    /// Drop all unexpired confirmation grants. Call when the operator's
    /// session ends or the context that produced the approvals is stale;
    /// consumed grants are already gone.
    pub fn clear_confirmations(&mut self) {
        self.grants.clear();
    }

    /// Install the operator prompt for `NeedsConfirm` actions. The hook is
    /// consulted at most once per `execute()`, at the first unconfirmed
    /// action, and receives the batch plus every `(index, reason)` still
    /// awaiting confirmation; it returns the indices the operator approved.
    /// Approved indices become one-shot grants, so the batch continues in
    /// order — actions before the confirmation point have already run,
    /// actions after it have not. The hook runs on whatever thread called
    /// `execute()`, hence `Send`. Remove with
    /// [`Executor::clear_confirm_hook`].
    pub fn set_confirm_hook(
        &mut self,
        hook: impl Fn(&Batch, &[(usize, String)]) -> Vec<usize> + Send + 'static,
    ) {
        self.confirm_hook = Some(Box::new(hook));
    }

    /// Remove the operator prompt installed by
    /// [`Executor::set_confirm_hook`]. Unconfirmed actions fail in place
    /// again; existing grants are untouched.
    pub fn clear_confirm_hook(&mut self) {
        self.confirm_hook = None;
    }

    /// A cloneable handle that arms emergency cancellation (see
    /// [`CancelHandle`]).
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            flag: Arc::clone(&self.cancel),
        }
    }

    /// Arm emergency cancellation directly (host shorthand for
    /// `cancel_handle().cancel()`).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation is currently armed.
    pub fn cancel_armed(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
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
    /// The safety policy is reviewed first: denied actions fail in place
    /// without aborting the batch. Actions the guard flags `NeedsConfirm`
    /// run only when the operator approved them — via a one-shot
    /// [`Executor::grant_confirmation`] or the
    /// [`Executor::set_confirm_hook`] prompt — and fail in place otherwise.
    ///
    /// Emergency cancellation is checked at every action boundary: if the
    /// cancel flag is armed, the current and all remaining actions drain as
    /// [`ExecError::Cancelled`] without touching backends, and the flag is
    /// cleared so it can't poison the next batch.
    ///
    /// Every exit runs the stuck-input sweep: buttons the executor pressed
    /// but never released (a release that failed after retries, or a
    /// non-retryable backend error mid-press) are released best-effort and
    /// reported on [`BatchResult::stuck_released`] / `stuck_unreleased`.
    pub fn execute(&mut self, batch: &Batch) -> BatchResult {
        let verdicts = crate::guard::review_batch(&self.policy, batch);
        self.hook_consulted = false;
        let mut result = BatchResult::new();
        for (action, verdict) in batch.0.iter().zip(verdicts.iter()) {
            if self.cancel.load(Ordering::SeqCst) {
                // One-shot: clear first so even a panic between here and
                // the return can't leave the flag armed for a later batch.
                self.cancel.store(false, Ordering::SeqCst);
                let remaining = batch.0.len() - result.outcomes.len();
                result.outcomes.extend(
                    std::iter::repeat(ActionOutcome::Failed {
                        error: ExecError::Cancelled(
                            "batch cancelled: emergency stop requested".into(),
                        ),
                    })
                    .take(remaining),
                );
                // The sweep touches backends, but only when an earlier
                // action in *this* batch left something held; an unstarted
                // batch holds nothing, so the no-backend-touch guarantee
                // for pure cancellation still holds.
                self.sweep_stuck(&mut result);
                return result;
            }
            let outcome = match verdict {
                crate::guard::Verdict::Allow => match self.run_allowed(batch, action, &mut result)
                {
                    Some(o) => o,
                    None => return result,
                },
                crate::guard::Verdict::Deny { reason } => ActionOutcome::Failed {
                    error: ExecError::Guard(reason.clone()),
                },
                crate::guard::Verdict::NeedsConfirm { reason } => {
                    if self.confirm_action(batch, &verdicts, action) {
                        match self.run_allowed(batch, action, &mut result) {
                            Some(o) => o,
                            None => return result,
                        }
                    } else {
                        ActionOutcome::Failed {
                            error: ExecError::NeedsConfirm {
                                reason: reason.clone(),
                            },
                        }
                    }
                }
            };
            result.outcomes.push(outcome);
        }
        self.sweep_stuck(&mut result);
        result
    }

    /// Run one guard-allowed (or confirmed) action: rate-limit check, then
    /// [`Executor::execute_one`]. Returns `None` when the rate limiter
    /// drained the batch — the result is already finalized (sweep run) and
    /// the caller must return it immediately.
    fn run_allowed(
        &mut self,
        batch: &Batch,
        action: &Action,
        result: &mut BatchResult,
    ) -> Option<ActionOutcome> {
        // Rate limit is checked after the guard: denied actions
        // touch no backend and shouldn't cost a token. Confirmed actions
        // do run, so they cost one like any allowed action.
        let limited = self
            .rate_limit
            .as_mut()
            .and_then(|lim| lim.try_acquire(self.clock.now()).err());
        match limited {
            None => {
                let (outcome, new_frame) = self.execute_one(action);
                if let Some(id) = new_frame {
                    result.new_frames.push(id);
                }
                Some(outcome)
            }
            Some(retry_after) => {
                // Drain the current and all remaining actions
                // as rate-limited without touching backends.
                let remaining = batch.0.len() - result.outcomes.len();
                result.outcomes.extend(
                    std::iter::repeat(ActionOutcome::Failed {
                        error: ExecError::RateLimited { retry_after },
                    })
                    .take(remaining),
                );
                self.sweep_stuck(result);
                None
            }
        }
    }

    /// Resolve a `NeedsConfirm` verdict against the confirmation channel.
    /// A stored grant for this exact action content authorizes it one-shot;
    /// otherwise the confirm hook — consulted at most once per batch — is
    /// asked for the operator's decision and its approvals become grants.
    fn confirm_action(
        &mut self,
        batch: &Batch,
        verdicts: &[crate::guard::Verdict],
        action: &Action,
    ) -> bool {
        let key = confirmation_key(action);
        if self.grants.remove(&key) {
            return true;
        }
        if self.hook_consulted {
            return false;
        }
        self.hook_consulted = true;
        let pending: Vec<(usize, String)> = verdicts
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v {
                crate::guard::Verdict::NeedsConfirm { reason } => Some((i, reason.clone())),
                _ => None,
            })
            .collect();
        let approved = match self.confirm_hook.as_ref() {
            Some(hook) => hook(batch, &pending),
            None => Vec::new(),
        };
        for i in approved {
            // Out-of-range or non-confirm indices are ignored: only a
            // NeedsConfirm verdict on matching content can consume a grant.
            if let Some(a) = batch.0.get(i) {
                self.grants.insert(confirmation_key(a));
            }
        }
        self.grants.remove(&key)
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
                |input, clock, t, u, _held| {
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
                self.coordinate_action(*frame, PxPoint { x: *x, y: *y }, |input, clock, t, u, held| {
                    Self::retry(t, || {
                        input.move_to(u)?;
                        clock.sleep(t.move_settle);
                        Self::tracked_press(&mut *input, &mut *held, button)?;
                        clock.sleep(t.press_hold);
                        Self::tracked_release(&mut *input, &mut *held, button)?;
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
                self.coordinate_action(*frame, PxPoint { x: *x, y: *y }, |input, clock, t, u, held| {
                    Self::retry(t, || {
                        for _ in 0..2 {
                            input.move_to(u)?;
                            clock.sleep(t.move_settle);
                            Self::tracked_press(&mut *input, &mut *held, button)?;
                            clock.sleep(t.press_hold);
                            Self::tracked_release(&mut *input, &mut *held, button)?;
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
                self.coordinate_action(*frame, PxPoint { x: *x, y: *y }, |input, clock, t, u, _held| {
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
            Action::ListWindows {
                title_substr,
                app_id,
            } => {
                let query = WindowQuery {
                    title_substr: title_substr.clone(),
                    app_id: app_id.clone(),
                    id: None,
                };
                let t = self.timing;
                match Self::retry(t, || self.window.find_window(&query)) {
                    Ok(windows) => (
                        ActionOutcome::Windows {
                            windows,
                            focused: None,
                        },
                        None,
                    ),
                    Err(e) => (ActionOutcome::Failed { error: e }, None),
                }
            }
            Action::ActiveWindow => {
                let t = self.timing;
                match Self::retry(t, || self.window.active_window()) {
                    Ok(active) => (
                        ActionOutcome::Windows {
                            windows: active.into_iter().collect(),
                            focused: None,
                        },
                        None,
                    ),
                    Err(e) => (ActionOutcome::Failed { error: e }, None),
                }
            }
            Action::FocusWindow { id } => {
                let t = self.timing;
                match Self::retry(t, || self.window.focus_window(*id)) {
                    Ok(focused) => (
                        ActionOutcome::Windows {
                            windows: Vec::new(),
                            focused: Some(focused),
                        },
                        None,
                    ),
                    Err(e) => (ActionOutcome::Failed { error: e }, None),
                }
            }
            // Reported as a ≤1-element window list (the bounds travel in the
            // window's `bounds` field); goes through find_window so the
            // backend's list-based default covers it.
            Action::WindowBounds { id } => {
                let query = WindowQuery {
                    id: Some(*id),
                    ..WindowQuery::default()
                };
                let t = self.timing;
                match Self::retry(t, || self.window.find_window(&query)) {
                    Ok(windows) => (
                        ActionOutcome::Windows {
                            windows,
                            focused: None,
                        },
                        None,
                    ),
                    Err(e) => (ActionOutcome::Failed { error: e }, None),
                }
            }
        }
    }

    /// Record a press against the held set: if the matching release never
    /// completes (infra retries exhausted, or a non-retryable backend
    /// error), the end-of-batch sweep knows to release it. Re-pressing an
    /// already-held button is a no-op for the set and idempotent on evdev,
    /// so retry closures can call this on every attempt.
    fn tracked_press(
        input: &mut I,
        held: &mut Vec<MouseButton>,
        button: MouseButton,
    ) -> Result<(), ExecError> {
        input.press(button)?;
        if !held.contains(&button) {
            held.push(button);
        }
        Ok(())
    }

    /// Release and drop from the held set. A failed release leaves the
    /// button in `held` for the end-of-batch sweep.
    fn tracked_release(
        input: &mut I,
        held: &mut Vec<MouseButton>,
        button: MouseButton,
    ) -> Result<(), ExecError> {
        match input.release(button) {
            Ok(()) => {
                held.retain(|b| *b != button);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// End-of-batch stuck-input sweep: release anything `tracked_press`
    /// recorded that never got its matching release. Best-effort: one
    /// release attempt per held button (no sleeps, no retries — the batch
    /// is over), then the backend's `release_all` as a last resort. The
    /// outcome lands on the result's `stuck_released` / `stuck_unreleased`
    /// so the model and the debug log see exactly what happened.
    fn sweep_stuck(&mut self, result: &mut BatchResult) {
        if self.held.is_empty() {
            return;
        }
        for button in std::mem::take(&mut self.held) {
            match self.input.release(button) {
                Ok(()) => result.stuck_released.push(button.name().to_string()),
                Err(_) => {
                    // Last resort. `release_all` errors by default, so an
                    // Ok here means the backend really did release
                    // everything it could hold — including this button.
                    if self.input.release_all().is_ok() {
                        result.stuck_released.push(button.name().to_string());
                    } else {
                        result.stuck_unreleased.push(button.name().to_string());
                    }
                }
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
        run: impl FnOnce(&mut I, &K, Timing, UInputAbs, &mut Vec<MouseButton>) -> Result<(), ExecError>,
    ) -> (ActionOutcome, Option<FrameId>) {
        match self.resolve(p, frame) {
            Ok((desktop, uinput)) => {
                let t = self.timing;
                match run(
                    &mut self.input,
                    &self.clock,
                    t,
                    uinput,
                    &mut self.held,
                ) {
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
            Self::tracked_press(&mut self.input, &mut self.held, MouseButton::Left)?;
            for p in &steps {
                self.input.move_to(*p)?;
                self.clock.sleep(t.drag_step_delay);
            }
            Self::tracked_release(&mut self.input, &mut self.held, MouseButton::Left)?;
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
    use crate::backend::{InputOp, MockCapture, MockInput, MockWindow, WindowBounds, WindowInfo};
    use crate::guard::Policy;

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

    #[test]
    fn guard_denial_fails_in_place_and_batch_continues() {
        let mut ex = executor();
        let r = ex.execute(&Batch(vec![
            Action::Keypress {
                keys: vec!["ctrl".into(), "alt".into(), "del".into()],
            },
            Action::Wait { ms: 5 },
        ]));
        assert!(!r.all_ok());
        assert!(matches!(
            r.outcomes[0],
            ActionOutcome::Failed {
                error: ExecError::Guard(_)
            }
        ));
        // Guard failures are not infra failures: never retried, batch lived on.
        if let ActionOutcome::Failed { error } = &r.outcomes[0] {
            assert!(!error.retryable());
        } else {
            panic!("expected failure");
        }
        assert!(matches!(r.outcomes[1], ActionOutcome::NoOp));
        // Nothing was sent to the input backend for the denied keypress.
        assert!(ex.input().log.is_empty());
    }

    #[test]
    fn guard_needs_confirm_is_a_guard_failure() {
        let mut ex = executor();
        let r = ex.execute(&Batch(vec![Action::Type {
            text: "rm -rf ~/tmp".into(),
        }]));
        assert!(matches!(
            r.outcomes[0],
            ActionOutcome::Failed {
                error: ExecError::NeedsConfirm { .. }
            }
        ));
        assert!(ex.input().log.is_empty());
    }

    fn destructive_type(text: &str) -> Action {
        Action::Type { text: text.into() }
    }

    #[test]
    fn grant_confirmation_runs_the_exact_action() {
        let mut ex = executor();
        let action = destructive_type("rm -rf ~/tmp");
        ex.grant_confirmation(&action);
        let r = ex.execute(&Batch(vec![action]));
        assert!(r.all_ok());
        assert!(ex
            .input()
            .log
            .contains(&InputOp::Type("rm -rf ~/tmp".into())));
    }

    #[test]
    fn grants_are_single_use() {
        let mut ex = executor();
        let a = destructive_type("rm -rf ~/tmp");
        ex.grant_confirmation(&a);
        let r = ex.execute(&Batch(vec![a.clone(), a]));
        // First consumed the grant and ran; the identical second action has
        // no grant left and fails as NeedsConfirm.
        assert!(matches!(r.outcomes[0], ActionOutcome::Done { .. }));
        assert!(matches!(
            r.outcomes[1],
            ActionOutcome::Failed {
                error: ExecError::NeedsConfirm { .. }
            }
        ));
        assert_eq!(
            ex.input()
                .log
                .iter()
                .filter(|op| matches!(op, InputOp::Type(_)))
                .count(),
            1
        );
    }

    #[test]
    fn grants_are_exact_content() {
        let mut ex = executor();
        ex.grant_confirmation(&destructive_type("rm -rf /tmp/cache"));
        // A different destructive string is NOT covered by the grant.
        let r = ex.execute(&Batch(vec![destructive_type("rm -rf /")]));
        assert!(matches!(
            r.outcomes[0],
            ActionOutcome::Failed {
                error: ExecError::NeedsConfirm { .. }
            }
        ));
        assert!(ex.input().log.is_empty());
    }

    #[test]
    fn clear_confirmations_drops_unexpired_grants() {
        let mut ex = executor();
        let a = destructive_type("rm -rf ~/tmp");
        ex.grant_confirmation(&a);
        ex.clear_confirmations();
        let r = ex.execute(&Batch(vec![a]));
        assert!(matches!(
            r.outcomes[0],
            ActionOutcome::Failed {
                error: ExecError::NeedsConfirm { .. }
            }
        ));
        assert!(ex.input().log.is_empty());
    }

    #[test]
    fn confirm_hook_approves_mid_batch_in_order() {
        let mut ex = executor();
        ex.set_confirm_hook(|batch, pending| {
            // The hook sees the batch and every (index, reason) awaiting
            // confirmation: here one destructive type at index 1.
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].0, 1);
            assert!(matches!(batch.0[1], Action::Type { .. }));
            vec![1]
        });
        let r = ex.execute(&Batch(vec![
            Action::Wait { ms: 10 },
            destructive_type("rm -rf ~/tmp"),
            Action::Wait { ms: 10 },
        ]));
        assert!(r.all_ok());
        assert!(ex
            .input()
            .log
            .contains(&InputOp::Type("rm -rf ~/tmp".into())));
    }

    #[test]
    fn confirm_hook_consulted_once_per_batch() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        let mut ex = executor();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        ex.set_confirm_hook(move |_batch, pending| {
            calls2.fetch_add(1, Ordering::SeqCst);
            assert_eq!(pending.len(), 2);
            // Approve only the first; the second must still fail.
            vec![pending[0].0]
        });
        let r = ex.execute(&Batch(vec![
            destructive_type("rm -rf ~/tmp"),
            destructive_type("dd if=/dev/zero of=/dev/sda"),
        ]));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(r.outcomes[0], ActionOutcome::Done { .. }));
        assert!(matches!(
            r.outcomes[1],
            ActionOutcome::Failed {
                error: ExecError::NeedsConfirm { .. }
            }
        ));
    }

    #[test]
    fn confirm_hook_decline_fails_in_place() {
        let mut ex = executor();
        ex.set_confirm_hook(|_batch, _pending| Vec::new());
        let r = ex.execute(&Batch(vec![
            Action::Wait { ms: 10 },
            destructive_type("rm -rf ~/tmp"),
        ]));
        assert!(matches!(r.outcomes[0], ActionOutcome::NoOp));
        assert!(matches!(
            r.outcomes[1],
            ActionOutcome::Failed {
                error: ExecError::NeedsConfirm { .. }
            }
        ));
        assert!(r.pending_confirmations().len() == 1);
        assert_eq!(r.pending_confirmations()[0].0, 1);
    }

    #[test]
    fn confirmed_action_still_costs_a_rate_token() {
        let mut ex = executor();
        ex.set_rate_limit(Some(crate::rate::RateLimit {
            capacity: 1,
            refill_per_sec: 0.0,
        }));
        let a = destructive_type("rm -rf ~/tmp");
        ex.grant_confirmation(&a);
        // The grant lets it run, but the bucket holds one token: the
        // confirmed action consumes it, and the next allowed action drains.
        let r = ex.execute(&Batch(vec![a, Action::Wait { ms: 10 }]));
        assert!(matches!(r.outcomes[0], ActionOutcome::Done { .. }));
        assert!(matches!(
            r.outcomes[1],
            ActionOutcome::Failed {
                error: ExecError::RateLimited { .. }
            }
        ));
    }

    #[test]
    fn read_only_blocks_input_but_screenshots_still_register_frames() {
        let mut ex = executor();
        ex.set_read_only(true);
        // Screenshots sail through and register frames as usual.
        let r = ex.execute(&screenshot_batch());
        assert!(r.all_ok());
        assert_eq!(r.new_frames, vec![FrameId(1)]);
        // Input actions are denied in place with guard errors — even on a
        // *valid* frame, so the guard verdict demonstrably fires before any
        // mapping or backend work — and the batch keeps going past them.
        let r = ex.execute(&Batch(vec![
            Action::Click {
                frame: FrameId(1),
                button: MouseButton::Left,
                x: 10,
                y: 10,
            },
            Action::Type {
                text: "hello".into(),
            },
            Action::Wait { ms: 50 },
        ]));
        assert_eq!(r.outcomes.len(), 3);
        assert!(r.outcomes.iter().all(|o| matches!(
            o,
            ActionOutcome::Failed {
                error: ExecError::Guard(_)
            }
        )));
        assert!(!r.all_ok());
        assert!(ex.input().log.is_empty()); // backends never consulted
        // Toggling back off restores full execution.
        ex.set_read_only(false);
        let r = ex.execute(&Batch(vec![Action::Click {
            frame: FrameId(1),
            button: MouseButton::Left,
            x: 10,
            y: 10,
        }]));
        assert!(r.all_ok());
        assert_eq!(ex.input().log.len(), 3); // move + press + release
    }

    #[test]
    fn cancel_before_batch_drains_every_action_without_touching_backends() {
        let mut ex = executor();
        ex.cancel(); // armed before the batch
        assert!(ex.cancel_armed());
        let r = ex.execute(&Batch(vec![
            Action::Screenshot { note: None },
            Action::Wait { ms: 5 },
            Action::Keypress {
                keys: vec!["a".into()],
            },
        ]));
        assert_eq!(r.outcomes.len(), 3);
        for o in &r.outcomes {
            match o {
                ActionOutcome::Failed {
                    error: ExecError::Cancelled(_),
                } => {}
                other => panic!("expected Cancelled, got {:?}", other),
            }
        }
        assert!(!r.all_ok());
        assert!(ex.input().log.is_empty());
        assert!(ex.clock().sleeps().is_empty());
        // One-shot: the flag clears itself so the next batch runs clean.
        assert!(!ex.cancel_armed());
        let r = ex.execute(&screenshot_batch());
        assert!(r.all_ok());
        assert_eq!(r.new_frames, vec![FrameId(1)]);
    }

    /// Input-backend wrapper that arms executor cancellation the first time
    /// the pointer is pressed — simulates an external
    /// `notifications/cancelled` arriving mid-batch. The handle is handed
    /// over after the executor is built (shared slot), so the wrapper can
    /// flip the *executor's own* flag.
    struct CancellingInput {
        inner: MockInput,
        slot: std::sync::Arc<std::sync::Mutex<Option<CancelHandle>>>,
        flipped: bool,
    }

    impl InputBackend for CancellingInput {
        fn move_to(&mut self, p: UInputAbs) -> Result<(), ExecError> {
            self.inner.move_to(p)
        }
        fn press(&mut self, b: MouseButton) -> Result<(), ExecError> {
            if !self.flipped {
                self.flipped = true;
                if let Some(h) = self.slot.lock().unwrap().as_ref() {
                    h.cancel();
                }
            }
            self.inner.press(b)
        }
        fn release(&mut self, b: MouseButton) -> Result<(), ExecError> {
            self.inner.release(b)
        }
        fn wheel(&mut self, dx: f64, dy: f64) -> Result<(), ExecError> {
            self.inner.wheel(dx, dy)
        }
        fn keypress(&mut self, keys: &[String]) -> Result<(), ExecError> {
            self.inner.keypress(keys)
        }
        fn type_text(&mut self, text: &str) -> Result<(), ExecError> {
            self.inner.type_text(text)
        }
    }

    #[test]
    fn cancel_mid_batch_stops_at_the_next_action_boundary() {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut ex = Executor::new(
            MockCapture::new(identity_space()),
            CancellingInput {
                inner: MockInput::new(),
                slot: slot.clone(),
                flipped: false,
            },
            MockWindow::default(),
            MockClock::new(),
            Timing::default(),
            DesktopGeometry::single(1920.0, 1080.0),
        );
        *slot.lock().unwrap() = Some(ex.cancel_handle());

        let r = ex.execute(&Batch(vec![
            Action::Screenshot { note: None },
            Action::Click {
                frame: FrameId(1),
                button: MouseButton::Left,
                x: 10,
                y: 10,
            },
            Action::Wait { ms: 5 },
            Action::Type {
                text: "never typed".into(),
            },
        ]));
        // Screenshot + click completed (the click's press armed the flag;
        // cancellation takes effect at the *next* boundary, so the in-flight
        // action runs to completion). Wait and Type drained as cancelled.
        assert_eq!(r.new_frames, vec![FrameId(1)]);
        assert!(matches!(r.outcomes[0], ActionOutcome::Done { .. }));
        assert!(matches!(r.outcomes[1], ActionOutcome::Done { .. }));
        for o in &r.outcomes[2..] {
            assert!(
                matches!(
                    o,
                    ActionOutcome::Failed {
                        error: ExecError::Cancelled(_)
                    }
                ),
                "expected Cancelled, got {:?}",
                o
            );
        }
        assert!(!ex.cancel_armed()); // one-shot: cleared
        // The following batch runs normally.
        let r = ex.execute(&Batch(vec![Action::Wait { ms: 5 }]));
        assert!(r.all_ok());
    }

    #[test]
    fn cancel_handle_clone_arms_the_same_flag() {
        let ex = executor();
        let h1 = ex.cancel_handle();
        let h2 = h1.clone();
        assert!(!h1.is_armed());
        h2.cancel();
        assert!(h1.is_armed());
        assert!(ex.cancel_armed());
        h1.clear();
        assert!(!ex.cancel_armed());
    }

    #[test]
    fn custom_policy_replaces_default() {
        let mut ex = executor();
        // Permissive policy: the default would deny ctrl+alt+del.
        ex.set_policy(Policy {
            denied_keypresses: vec![],
            deny_tty_switch: false,
            ..Policy::default()
        });
        let r = ex.execute(&Batch(vec![Action::Keypress {
            keys: vec!["ctrl".into(), "alt".into(), "del".into()],
        }]));
        assert!(r.all_ok());
    }

    /// A clock the test advances by hand: sleeps are recorded, time is set.
    struct ManualClock {
        sleeps: std::cell::RefCell<Vec<Duration>>,
        now: std::cell::Cell<Instant>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                sleeps: std::cell::RefCell::new(Vec::new()),
                now: std::cell::Cell::new(Instant::now()),
            }
        }
        fn advance(&self, d: Duration) {
            self.now.set(self.now.get() + d);
        }
    }

    impl Clock for ManualClock {
        fn sleep(&self, d: Duration) {
            self.sleeps.borrow_mut().push(d);
        }
        fn now(&self) -> Instant {
            self.now.get()
        }
    }

    fn limited_executor(
        clock: ManualClock,
        limit: crate::rate::RateLimit,
    ) -> Executor<MockCapture, MockInput, MockWindow, ManualClock> {
        let mut ex = Executor::new(
            MockCapture::new(identity_space()),
            MockInput::new(),
            MockWindow::default(),
            clock,
            Timing::default(),
            DesktopGeometry::single(1920.0, 1080.0),
        );
        ex.set_rate_limit(Some(limit));
        ex
    }

    fn type_batch(n: usize) -> Batch {
        Batch(
            (0..n)
                .map(|i| Action::Type {
                    text: format!("t{}", i),
                })
                .collect(),
        )
    }

    fn is_rate_limited(o: &ActionOutcome) -> bool {
        matches!(
            o,
            ActionOutcome::Failed {
                error: ExecError::RateLimited { .. },
            }
        )
    }

    #[test]
    fn rate_limit_drains_tail_without_touching_backends() {
        let clock = ManualClock::new();
        let mut ex = limited_executor(
            clock,
            crate::rate::RateLimit {
                capacity: 3,
                refill_per_sec: 0.0,
            },
        );
        let r = ex.execute(&type_batch(5));
        assert_eq!(r.outcomes.len(), 5);
        assert!(matches!(
            &r.outcomes[..3],
            [ActionOutcome::Done { .. }, ActionOutcome::Done { .. }, ActionOutcome::Done { .. }]
        ));
        assert!(is_rate_limited(&r.outcomes[3]));
        assert!(is_rate_limited(&r.outcomes[4]));
        // The drained tail never reached the input backend.
        assert_eq!(ex.input().log.len(), 3);
    }

    #[test]
    fn rate_limit_retry_after_is_none_when_frozen() {
        let clock = ManualClock::new();
        let mut ex = limited_executor(
            clock,
            crate::rate::RateLimit {
                capacity: 1,
                refill_per_sec: 0.0,
            },
        );
        let r = ex.execute(&type_batch(2));
        assert!(matches!(
            &r.outcomes[1],
            ActionOutcome::Failed {
                error: ExecError::RateLimited { retry_after: None },
            }
        ));
    }

    #[test]
    fn rate_limit_refills_with_time() {
        let clock = ManualClock::new();
        let mut ex = limited_executor(
            clock,
            crate::rate::RateLimit {
                capacity: 2,
                refill_per_sec: 10.0,
            },
        );
        let r = ex.execute(&type_batch(3));
        assert!(is_rate_limited(&r.outcomes[2]));
        // 0.2s at 10/s = 2 tokens: a fresh batch runs again.
        ex.clock().advance(Duration::from_millis(200));
        let r = ex.execute(&type_batch(2));
        assert!(r.all_ok());
    }

    #[test]
    fn rate_limit_disabled_is_unlimited() {
        let clock = ManualClock::new();
        let mut ex = limited_executor(clock, crate::rate::RateLimit::default());
        ex.set_rate_limit(None);
        // Well past the default 20-burst: everything runs.
        let r = ex.execute(&type_batch(25));
        assert!(r.all_ok());
        assert_eq!(ex.input().log.len(), 25);
    }

    #[test]
    fn guard_denied_actions_do_not_consume_tokens() {
        let clock = ManualClock::new();
        let mut ex = limited_executor(
            clock,
            crate::rate::RateLimit {
                capacity: 1,
                refill_per_sec: 0.0,
            },
        );
        // The denied keypress costs nothing; the one token is spent on Type.
        let r = ex.execute(&Batch(vec![
            Action::Keypress {
                keys: vec!["CTRL".into(), "ALT".into(), "DEL".into()],
            },
            Action::Type {
                text: "hi".into(),
            },
        ]));
        assert!(matches!(
            &r.outcomes[0],
            ActionOutcome::Failed {
                error: ExecError::Guard(_),
            }
        ));
        assert!(matches!(&r.outcomes[1], ActionOutcome::Done { .. }));
    }

    fn windowed_executor() -> Executor<MockCapture, MockInput, MockWindow, MockClock> {
        let windows = vec![
            WindowInfo {
                id: 101,
                title: "Terminal - zsh".into(),
                app_id: "org.kde.konsole".into(),
                focused: true,
                bounds: Some(WindowBounds {
                    x: 0.0,
                    y: 0.0,
                    w: 960.0,
                    h: 1080.0,
                }),
            },
            WindowInfo {
                id: 202,
                title: "Firefox".into(),
                app_id: "org.mozilla.firefox".into(),
                focused: false,
                bounds: Some(WindowBounds {
                    x: 960.0,
                    y: 0.0,
                    w: 960.0,
                    h: 1080.0,
                }),
            },
        ];
        Executor::new(
            MockCapture::new(identity_space()),
            MockInput::new(),
            MockWindow::new(windows),
            MockClock::new(),
            Timing::default(),
            DesktopGeometry::single(1920.0, 1080.0),
        )
    }

    fn windows_outcome(r: &BatchResult) -> (&[WindowInfo], Option<bool>) {
        match &r.outcomes[0] {
            ActionOutcome::Windows { windows, focused } => (windows, *focused),
            other => panic!("expected Windows outcome, got {other:?}"),
        }
    }

    #[test]
    fn list_windows_returns_everything_unfiltered() {
        let mut ex = windowed_executor();
        let r = ex.execute(&Batch(vec![Action::ListWindows {
            title_substr: None,
            app_id: None,
        }]));
        assert!(r.all_ok());
        let (windows, focused) = windows_outcome(&r);
        assert_eq!(focused, None);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].title, "Terminal - zsh");
        assert!(windows[0].focused);
    }

    #[test]
    fn list_windows_filters_by_title_substring() {
        let mut ex = windowed_executor();
        let r = ex.execute(&Batch(vec![Action::ListWindows {
            title_substr: Some("fire".into()),
            app_id: None,
        }]));
        let (windows, _) = windows_outcome(&r);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 202);
    }

    #[test]
    fn list_windows_filters_by_app_id() {
        let mut ex = windowed_executor();
        let r = ex.execute(&Batch(vec![Action::ListWindows {
            title_substr: None,
            app_id: Some("org.kde.konsole".into()),
        }]));
        let (windows, _) = windows_outcome(&r);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 101);
    }

    #[test]
    fn active_window_reports_only_the_focused_one() {
        let mut ex = windowed_executor();
        let r = ex.execute(&Batch(vec![Action::ActiveWindow]));
        let (windows, _) = windows_outcome(&r);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 101);
    }

    #[test]
    fn focus_window_flips_focus_and_reports_success() {
        let mut ex = windowed_executor();
        let r = ex.execute(&Batch(vec![Action::FocusWindow { id: 202 }]));
        let (windows, focused) = windows_outcome(&r);
        assert_eq!(focused, Some(true));
        assert!(windows.is_empty());
        // The backend state actually flipped: active_window follows.
        let r = ex.execute(&Batch(vec![Action::ActiveWindow]));
        let (windows, _) = windows_outcome(&r);
        assert_eq!(windows[0].id, 202);
    }

    #[test]
    fn focus_window_unknown_id_reports_false_without_touching_state() {
        let mut ex = windowed_executor();
        let r = ex.execute(&Batch(vec![Action::FocusWindow { id: 999 }]));
        let (windows, focused) = windows_outcome(&r);
        assert_eq!(focused, Some(false));
        assert!(windows.is_empty());
        // Nothing was touched: the previously focused window is still active.
        let r = ex.execute(&Batch(vec![Action::ActiveWindow]));
        let (windows, _) = windows_outcome(&r);
        assert_eq!(windows[0].id, 101);
    }

    #[test]
    fn window_bounds_returns_one_element_list_with_geometry() {
        let mut ex = windowed_executor();
        let r = ex.execute(&Batch(vec![Action::WindowBounds { id: 202 }]));
        let (windows, focused) = windows_outcome(&r);
        assert_eq!(focused, None);
        assert_eq!(windows.len(), 1);
        assert_eq!(
            windows[0].bounds,
            Some(WindowBounds {
                x: 960.0,
                y: 0.0,
                w: 960.0,
                h: 1080.0,
            })
        );
        // Unknown id: empty list, still ok.
        let r = ex.execute(&Batch(vec![Action::WindowBounds { id: 999 }]));
        let (windows, _) = windows_outcome(&r);
        assert!(windows.is_empty());
    }

    #[test]
    fn window_actions_are_guard_reviewed_like_any_action() {
        let mut ex = windowed_executor();
        ex.set_read_only(true);
        // Queries sail through read-only...
        let r = ex.execute(&Batch(vec![
            Action::ListWindows {
                title_substr: None,
                app_id: None,
            },
            Action::ActiveWindow,
            Action::WindowBounds { id: 101 },
        ]));
        assert!(r.all_ok());
        // ...but focus_window is a state change: denied in place.
        let r = ex.execute(&Batch(vec![Action::FocusWindow { id: 202 }]));
        assert!(matches!(
            &r.outcomes[0],
            ActionOutcome::Failed {
                error: ExecError::Guard(_),
            }
        ));
    }

    /// Input-backend wrapper with a configurable release-failure budget:
    /// the first `fail_budget` `release` calls fail with a non-retryable
    /// backend error (exercising the stuck-input sweep); `release_all`
    /// optionally fails too.
    struct FailingRelease {
        inner: MockInput,
        fail_budget: usize,
        fail_release_all: bool,
    }

    impl FailingRelease {
        fn new(fail_budget: usize, fail_release_all: bool) -> Self {
            Self {
                inner: MockInput::new(),
                fail_budget,
                fail_release_all,
            }
        }
    }

    impl InputBackend for FailingRelease {
        fn move_to(&mut self, p: UInputAbs) -> Result<(), ExecError> {
            self.inner.move_to(p)
        }
        fn press(&mut self, b: MouseButton) -> Result<(), ExecError> {
            self.inner.press(b)
        }
        fn release(&mut self, b: MouseButton) -> Result<(), ExecError> {
            if self.fail_budget > 0 {
                self.fail_budget -= 1;
                return Err(ExecError::Backend("release line is down".into()));
            }
            self.inner.release(b)
        }
        fn wheel(&mut self, dx: f64, dy: f64) -> Result<(), ExecError> {
            self.inner.wheel(dx, dy)
        }
        fn keypress(&mut self, keys: &[String]) -> Result<(), ExecError> {
            self.inner.keypress(keys)
        }
        fn type_text(&mut self, text: &str) -> Result<(), ExecError> {
            self.inner.type_text(text)
        }
        fn release_all(&mut self) -> Result<(), ExecError> {
            if self.fail_release_all {
                return Err(ExecError::Backend("release_all line is down".into()));
            }
            self.inner.release_all()
        }
    }

    fn stuck_executor(fail_budget: usize, fail_release_all: bool) -> Executor<MockCapture, FailingRelease, MockWindow, MockClock> {
        Executor::new(
            MockCapture::new(identity_space()),
            FailingRelease::new(fail_budget, fail_release_all),
            MockWindow::default(),
            MockClock::new(),
            Timing::default(),
            DesktopGeometry::single(1920.0, 1080.0),
        )
    }

    fn screenshot_then_click() -> Batch {
        Batch(vec![
            Action::Screenshot { note: None },
            Action::Click {
                frame: FrameId(1),
                button: MouseButton::Left,
                x: 10,
                y: 10,
            },
        ])
    }

    #[test]
    fn failed_release_is_swept_at_batch_end() {
        let mut ex = stuck_executor(1, false);
        let r = ex.execute(&screenshot_then_click());
        // The click failed (Backend errors aren't retried), leaving Left
        // held; the batch-end sweep released it.
        assert!(matches!(
            r.outcomes[1],
            ActionOutcome::Failed {
                error: ExecError::Backend(_)
            }
        ));
        assert_eq!(r.stuck_released, vec!["left".to_string()]);
        assert!(r.stuck_unreleased.is_empty());
        // Move, press, then the sweep's release (the failed release itself
        // never reached the log — the wrapper errored before delegating).
        let log = &ex.input().inner.log;
        assert_eq!(log.len(), 3);
        assert!(matches!(log[0], InputOp::Move(_)));
        assert_eq!(log[1], InputOp::Press(MouseButton::Left));
        assert_eq!(log[2], InputOp::Release(MouseButton::Left));
        // The held set is drained: the next batch issues no extra releases.
        let r2 = ex.execute(&Batch(vec![Action::Wait { ms: 1 }]));
        assert!(r2.all_ok());
        assert_eq!(ex.input().inner.log.len(), 3);
    }

    #[test]
    fn sweep_falls_back_to_release_all() {
        let mut ex = stuck_executor(usize::MAX, false);
        let r = ex.execute(&screenshot_then_click());
        assert_eq!(r.stuck_released, vec!["left".to_string()]);
        assert!(r.stuck_unreleased.is_empty());
        // Per-button release failed, then release_all cleaned up.
        assert!(matches!(
            ex.input().inner.log.last(),
            Some(InputOp::ReleaseAll)
        ));
    }

    #[test]
    fn sweep_reports_unreleased_when_release_all_fails() {
        let mut ex = stuck_executor(usize::MAX, true);
        let r = ex.execute(&screenshot_then_click());
        assert!(r.stuck_released.is_empty());
        assert_eq!(r.stuck_unreleased, vec!["left".to_string()]);
    }

    #[test]
    fn clean_batch_reports_no_stuck_inputs() {
        let mut ex = executor();
        let r = ex.execute(&screenshot_then_click());
        assert!(r.all_ok());
        assert!(r.stuck_released.is_empty());
        assert!(r.stuck_unreleased.is_empty());
    }
}
