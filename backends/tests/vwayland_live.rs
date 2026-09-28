//! Live integration test: the real `Executor -> backend` path against a
//! headless py-vwayland compositor running a pywayland test client.
//!
//! Opt-in: `cargo test --test vwayland_live -- --ignored --nocapture`.
//! Requires `python3` with `py-vwayland` and `pywayland` installed.
//!
//! What it proves:
//! 1. A `Click` through the executor lands the pointer where the client
//!    sees it (motion + button events in the client's event log).
//! 2. `Type` of `"Aa!"` delivers shifted characters correctly: the exact
//!    evdev sequence with shift-held flags in the event log.
//! 3. Screenshots taken through the executor reflect app-visible changes.
//! 4. Window queries report the single fullscreen app.
//!
//! It also answers, empirically: does an immediate screenshot after
//! typing already show the update, or is a render delay needed?

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pcu_backends::vwayland::{VWaylandCapture, VWaylandInput, VWaylandWindow};
use pcu_core::action::{Action, Batch, MouseButton};
use pcu_core::backend::{CaptureBackend, InputBackend, WindowBackend, WindowId};
use pcu_core::executor::{Executor, Timing};
use pcu_core::frame::DesktopGeometry;
use pcu_core::result::ActionOutcome;
use pcu_core::RealClock;

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

struct Harness {
    child: Child,
    sock: String,
    width: u32,
    height: u32,
    event_log: String,
}

impl Harness {
    fn spawn() -> Self {
        let prereq = Command::new("python3")
            .args(["-c", "import vwayland, pywayland"])
            .status();
        match prereq {
            Ok(s) if s.success() => {}
            _ => panic!("vwayland_live needs python3 with py-vwayland and pywayland installed"),
        }
        let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vwayland/spawn.py");
        let mut child = Command::new("python3")
            .arg(helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn tests/vwayland/spawn.py");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = BufReader::new(stdout).lines();
        let line = lines
            .next()
            .expect("helper printed a JSON line")
            .expect("read helper stdout");
        let v: serde_json::Value = serde_json::from_str(&line).expect("helper JSON");
        Self {
            child,
            sock: v["sock"].as_str().expect("sock").to_string(),
            width: v["width"].as_u64().expect("width") as u32,
            height: v["height"].as_u64().expect("height") as u32,
            event_log: v["event_log"].as_str().expect("event_log").to_string(),
        }
    }

    fn events(&self) -> Vec<String> {
        std::fs::read_to_string(&self.event_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Event-log lines appended after `mark` (a previous line count).
    fn events_since(&self, mark: usize) -> Vec<String> {
        self.events().into_iter().skip(mark).collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Closing stdin ends the helper, which kills the compositor.
        if let Some(mut stdin) = self.child.stdin.take() {
            let _ = stdin.write_all(b"quit\n");
        }
        let _ = self.child.wait();
    }
}

fn executor(
    h: &Harness,
    capture: VWaylandCapture,
    input: VWaylandInput,
    window: VWaylandWindow,
) -> Executor<VWaylandCapture, VWaylandInput, VWaylandWindow, RealClock> {
    Executor::new(
        capture,
        input,
        window,
        RealClock,
        Timing {
            // Keep the live test fast; the harness asserts settle-free
            // behavior explicitly where it matters.
            move_settle: Duration::from_millis(50),
            press_hold: Duration::from_millis(20),
            screenshot_settle: Duration::from_millis(100),
            ..Timing::default()
        },
        DesktopGeometry::single(h.width as f64, h.height as f64),
    )
}

fn assert_all_ok(outcomes: &[ActionOutcome], what: &str) {
    for (i, o) in outcomes.iter().enumerate() {
        assert!(
            matches!(o, ActionOutcome::Done { .. } | ActionOutcome::NoOp),
            "{what} action {i} failed: {o:?}"
        );
    }
}

#[test]
#[ignore = "needs a live py-vwayland compositor; run with -- --ignored"]
fn live_executor_round_trip() {
    let h = Harness::spawn();
    assert_eq!((h.width, h.height), (1280, 720), "helper dims");

    let mut capture = VWaylandCapture::probe(&h.sock).expect("probe");
    assert_eq!(capture.dimensions(), (1280, 720));
    let _ = capture.screenshot().expect("probe screenshot");
    let input = VWaylandInput::new(&h.sock, h.width, h.height);
    let mut window = VWaylandWindow::new(&h.sock, h.width, h.height);

    // ---- window backend: one fullscreen app ----
    let wins = window.list_windows().expect("list_windows");
    assert_eq!(wins.len(), 1, "exactly one app window");
    let w = &wins[0];
    assert_eq!(w.app_id, "vwayland");
    assert!(w.focused);
    let b = w.bounds.expect("fullscreen bounds");
    assert_eq!((b.x, b.y, b.w, b.h), (0.0, 0.0, 1280.0, 720.0));
    assert_eq!(window.active_window().expect("active").map(|a| a.id), Some(w.id));
    assert!(window.focus_window(w.id).expect("focus known id"));
    assert!(!window
        .focus_window(WindowId(u64::MAX))
        .expect("focus unknown id"));

    let mut ex = executor(&h, capture, input, window);

    // ---- screenshot through the executor ----
    let r = ex.execute(&Batch(vec![Action::Screenshot { note: None }]));
    assert_all_ok(&r.outcomes, "screenshot");
    assert_eq!(r.new_frames.len(), 1);
    let frame = r.new_frames[0];

    // ---- click lands where the client sees it ----
    let mark = h.events().len();
    let r = ex.execute(&Batch(vec![Action::Click {
        frame,
        x: 640,
        y: 360,
        button: MouseButton::Left,
    }]));
    assert_all_ok(&r.outcomes, "click");
    // Give the client a beat to dispatch pointer events.
    let deadline = Instant::now() + Duration::from_secs(5);
    let saw = loop {
        let ev = h.events_since(mark);
        // The first pointer_move into the surface arrives as
        // wl_pointer.enter, later ones as motion; accept either.
        let pos_ok = ev.iter().any(|l| {
            (l.starts_with("motion ") || l.starts_with("enter ")) && {
                let p: Vec<i32> =
                    l.split_whitespace().skip(1).map(|n| n.parse().unwrap()).collect();
                (p[0] - 640).abs() <= 2 && (p[1] - 360).abs() <= 2
            }
        });
        let button_ok = ev.iter().any(|l| l == "button 272");
        if pos_ok && button_ok {
            break true;
        }
        if Instant::now() > deadline {
            eprintln!("pointer events after click: {ev:?}");
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(saw, "client saw pointer at (640,360) + left-button press");

    // ---- shifted typing arrives correctly ----
    let shot_before = {
        let mut c = VWaylandCapture::probe(&h.sock).expect("re-probe");
        c.screenshot().expect("baseline shot").bytes
    };
    assert!(shot_before.starts_with(PNG_MAGIC), "screenshot is a PNG");

    let mark = h.events().len();
    let t0 = Instant::now();
    let r = ex.execute(&Batch(vec![Action::Type {
        text: "Aa!".to_string(),
    }]));
    assert_all_ok(&r.outcomes, "type");

    // Immediate screenshot: does it already show the typed keys?
    let mut c = VWaylandCapture::probe(&h.sock).expect("re-probe");
    let shot_fast = c.screenshot().expect("immediate shot").bytes;
    let fast_elapsed = t0.elapsed();
    std::thread::sleep(Duration::from_secs(1));
    let shot_slow = c.screenshot().expect("delayed shot").bytes;

    let changed_fast = shot_fast != shot_before;
    let changed_slow = shot_slow != shot_before;
    eprintln!(
        "render-lag probe: immediate shot ({fast_elapsed:?} after type) \
         changed={changed_fast}, delayed shot changed={changed_slow}"
    );
    assert!(
        changed_slow,
        "typing must be visible in a screenshot taken 1s later"
    );
    // The empirical answer to the render-delay question.
    assert!(
        changed_fast,
        "immediate screenshot after typing already reflects the update \
         (no render delay needed beyond the IPC round trip)"
    );

    // Exact key sequence the client received (pressed events only).
    // A = shift+30, a = 30, ! = shift+2. The client logs each press with
    // whether shift was already held at that moment.
    let deadline = Instant::now() + Duration::from_secs(5);
    let keys: Vec<String> = loop {
        let keys: Vec<String> = h
            .events_since(mark)
            .into_iter()
            .filter(|l| l.starts_with("key "))
            .collect();
        if keys.len() >= 5 || Instant::now() > deadline {
            break keys;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        keys,
        vec![
            "key 42 0", // shift press (shift not yet held)
            "key 30 1", // A
            "key 30 0", // a
            "key 42 0", // shift press
            "key 2 1",  // !
        ],
        "shifted typing delivered exact evdev sequence"
    );

    // ---- release_all is a safe no-op at rest (called directly; the
    // executor only reaches for it in its stuck-input sweep) ----
    VWaylandInput::new(&h.sock, h.width, h.height)
        .release_all()
        .expect("release_all");
}
