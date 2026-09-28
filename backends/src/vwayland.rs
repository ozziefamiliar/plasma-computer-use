//! Live-test backends over py-vwayland's Unix-socket JSON-line IPC.
//!
//! These backends drive a *virtual* Smithay compositor spawned by
//! [py-vwayland](https://github.com/llaa33219/py-vwayland) — the missing
//! link for live integration tests on machines without Plasma. They
//! implement the same three traits as the production backends, so the real
//! `Executor` path (guard → rate limit → backend → frame registry) runs
//! unmodified against a real compositor.
//!
//! Protocol (py-vwayland `client.py`): one Unix-socket connection per
//! command, one JSON line out, one JSON header line back
//! (`{"ok": true, ...}`); `screenshot` appends N raw PNG bytes after the
//! header (`"bytes": N`).
//!
//! Two empirical findings baked in here (2026-09-28, py-vwayland 0.1.1):
//! - Keycodes pass through to `wl_keyboard` clients **unchanged** — the
//!   compositor does *not* apply the +8 XKB offset the Wayland convention
//!   calls for (verified with a pywayland probe: sent evdev 42, received
//!   42). So this backend sends raw evdev codes, exactly like the Python
//!   client does.
//! - `pointer_axis` with `dy > 0` scrolls *up*; pcu's contract is positive
//!   = down/right, so [`VWaylandInput::wheel`] negates (the same inversion
//!   the KWin live test plan flags for real Plasma).
//!
//! Construction takes a socket path; the compositor is spawned outside the
//! Rust code (see `tests/vwayland/` for the Python spawn helper + the
//! pywayland test client).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pcu_core::backend::{
    CapturedFrame, CaptureBackend, InputBackend, WindowBackend, WindowBounds, WindowId,
    WindowInfo,
};
use pcu_core::coord::UInputAbs;
use pcu_core::frame::CoordSpace;
use pcu_core::result::ExecError;
use pcu_core::MouseButton;

use super::uinput::{KEY_LEFTSHIFT, KEY_RIGHTSHIFT, key_code, us_key};
use super::kwin::fnv1a;

const IPC_TIMEOUT: Duration = Duration::from_secs(10);
const SHOT_TIMEOUT: Duration = Duration::from_secs(60);

fn infra(e: impl std::fmt::Display) -> ExecError {
    ExecError::Infra(format!("vwayland ipc: {e}"))
}

fn backend_err(e: impl std::fmt::Display) -> ExecError {
    ExecError::Backend(format!("vwayland: {e}"))
}

/// One JSON-line IPC round trip over a fresh Unix-socket connection.
#[derive(Debug, Clone)]
struct Ipc {
    sock: PathBuf,
}

impl Ipc {
    fn connect(&self, timeout: Duration) -> Result<UnixStream, ExecError> {
        let s = UnixStream::connect(&self.sock).map_err(infra)?;
        s.set_read_timeout(Some(timeout)).map_err(infra)?;
        s.set_write_timeout(Some(timeout)).map_err(infra)?;
        Ok(s)
    }

    /// Send `payload`, return the parsed JSON header (`ok: true` checked).
    fn request(&self, payload: &serde_json::Value) -> Result<serde_json::Value, ExecError> {
        let mut s = self.connect(IPC_TIMEOUT)?;
        let line = serde_json::to_string(payload).map_err(backend_err)?;
        s.write_all(line.as_bytes()).map_err(infra)?;
        s.write_all(b"\n").map_err(infra)?;
        let header = read_header(&mut s)?;
        check_ok(&header)?;
        Ok(header)
    }

    /// Like [`Ipc::request`], but the header is followed by `bytes` raw
    /// payload bytes (the `screenshot` command). Both are read through
    /// one `BufReader`: a buffered header read may already hold payload
    /// bytes, so the payload must come from the same reader.
    fn request_bytes(
        &self,
        payload: &serde_json::Value,
    ) -> Result<(serde_json::Value, Vec<u8>), ExecError> {
        let mut s = self.connect(SHOT_TIMEOUT)?;
        let line = serde_json::to_string(payload).map_err(backend_err)?;
        s.write_all(line.as_bytes()).map_err(infra)?;
        s.write_all(b"\n").map_err(infra)?;
        let mut reader = BufReader::new(s);
        let header = read_header_line(&mut reader)?;
        check_ok(&header)?;
        let n = header
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| backend_err("screenshot header missing `bytes`"))?;
        let mut buf = vec![0u8; n as usize];
        reader.read_exact(&mut buf).map_err(infra)?;
        Ok((header, buf))
    }
}

fn read_header_line<R: Read>(reader: &mut BufReader<R>) -> Result<serde_json::Value, ExecError> {
    let mut line = String::new();
    reader.read_line(&mut line).map_err(infra)?;
    serde_json::from_str(&line).map_err(|e| backend_err(format!("bad ipc header: {e}")))
}

fn read_header(s: &mut UnixStream) -> Result<serde_json::Value, ExecError> {
    read_header_line(&mut BufReader::new(s))
}

fn check_ok(header: &serde_json::Value) -> Result<(), ExecError> {
    if header.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(());
    }
    let msg = header
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown compositor error");
    Err(backend_err(format!("compositor refused: {msg}")))
}

fn u64_field(v: &serde_json::Value, name: &str) -> Result<u64, ExecError> {
    v.get(name)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| backend_err(format!("header missing `{name}`")))
}

// ---- capture --------------------------------------------------------------

/// Fullscreen screenshots from the virtual compositor.
#[derive(Debug, Clone)]
pub struct VWaylandCapture {
    ipc: Ipc,
    width: u32,
    height: u32,
}

impl VWaylandCapture {
    /// Ping the compositor and learn the output dimensions.
    pub fn probe(sock_path: impl AsRef<Path>) -> Result<Self, ExecError> {
        let ipc = Ipc {
            sock: sock_path.as_ref().to_path_buf(),
        };
        let header = ipc.request(&serde_json::json!({"cmd": "ping"}))?;
        let width = u64_field(&header, "width")? as u32;
        let height = u64_field(&header, "height")? as u32;
        Ok(Self { ipc, width, height })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl CaptureBackend for VWaylandCapture {
    fn screenshot(&mut self) -> Result<CapturedFrame, ExecError> {
        let (header, png) = self
            .ipc
            .request_bytes(&serde_json::json!({"cmd": "screenshot"}))?;
        let w = u64_field(&header, "width")? as u32;
        let h = u64_field(&header, "height")? as u32;
        Ok(CapturedFrame {
            bytes: png,
            space: CoordSpace {
                image_w: w,
                image_h: h,
                origin_x: 0.0,
                origin_y: 0.0,
                scale_x: 1.0,
                scale_y: 1.0,
            },
        })
    }
}

// ---- input ----------------------------------------------------------------

/// Pointer + keyboard through the compositor's IPC.
#[derive(Debug, Clone)]
pub struct VWaylandInput {
    ipc: Ipc,
    width: f64,
    height: f64,
}

impl VWaylandInput {
    pub fn new(sock_path: impl AsRef<Path>, width: u32, height: u32) -> Self {
        Self {
            ipc: Ipc {
                sock: sock_path.as_ref().to_path_buf(),
            },
            width: width as f64,
            height: height as f64,
        }
    }

    fn key(&mut self, code: u16, pressed: bool) -> Result<(), ExecError> {
        self.ipc.request(
            &serde_json::json!({"cmd": "key", "code": code, "pressed": pressed}),
        )?;
        Ok(())
    }

    fn tap(&mut self, code: u16) -> Result<(), ExecError> {
        self.key(code, true)?;
        self.key(code, false)
    }

    /// Type one char the way py-vwayland's `type_text` does: US layout,
    /// shift held for uppercase and shifted punctuation.
    fn type_char(&mut self, c: char) -> Result<(), ExecError> {
        if c == '\n' || c == '\r' {
            return self.tap(key_code("ENTER").expect("ENTER in key table"));
        }
        if c == '\t' {
            return self.tap(key_code("TAB").expect("TAB in key table"));
        }
        let (code, shift) = us_key(c).ok_or_else(|| {
            backend_err(format!(
                "cannot type {c:?} (U+{:04X}) through raw keycodes",
                c as u32
            ))
        })?;
        if shift {
            self.key(KEY_LEFTSHIFT, true)?;
        }
        let r = self.tap(code);
        if shift {
            self.key(KEY_LEFTSHIFT, false)?;
        }
        r
    }
}

/// evdev BTN_* codes, matching py-vwayland's Python client (272/273/274).
fn button_code(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => 0x110,   // BTN_LEFT
        MouseButton::Right => 0x111,  // BTN_RIGHT
        MouseButton::Middle => 0x112, // BTN_MIDDLE
    }
}

impl InputBackend for VWaylandInput {
    fn move_to(&mut self, p: UInputAbs) -> Result<(), ExecError> {
        // The compositor takes logical float pixels; denormalize from the
        // executor's 0..=65535 uinput range using the pinged dimensions.
        let x = p.x as f64 / 65535.0 * self.width;
        let y = p.y as f64 / 65535.0 * self.height;
        self.ipc
            .request(&serde_json::json!({"cmd": "pointer_move", "x": x, "y": y}))?;
        Ok(())
    }

    fn press(&mut self, button: MouseButton) -> Result<(), ExecError> {
        self.ipc.request(
            &serde_json::json!({"cmd": "pointer_button", "button": button_code(button), "pressed": true}),
        )?;
        Ok(())
    }

    fn release(&mut self, button: MouseButton) -> Result<(), ExecError> {
        self.ipc.request(
            &serde_json::json!({"cmd": "pointer_button", "button": button_code(button), "pressed": false}),
        )?;
        Ok(())
    }

    fn wheel(&mut self, dx: f64, dy: f64) -> Result<(), ExecError> {
        // Protocol parity: dy > 0 scrolls *up*; pcu's contract is positive
        // = down/right, so negate.
        self.ipc
            .request(&serde_json::json!({"cmd": "pointer_axis", "dx": -dx, "dy": -dy}))?;
        Ok(())
    }

    fn keypress(&mut self, keys: &[String]) -> Result<(), ExecError> {
        let mut codes = Vec::with_capacity(keys.len());
        for name in keys {
            codes.push(key_code(name).ok_or_else(|| {
                backend_err(format!("unknown key name {name:?}"))
            })?);
        }
        for &code in &codes {
            self.key(code, true)?;
        }
        for &code in codes.iter().rev() {
            self.key(code, false)?;
        }
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), ExecError> {
        for c in text.chars() {
            self.type_char(c)?;
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), ExecError> {
        // Buttons first, then modifiers: mirrors UInputBackend's sweep.
        // type_text only ever holds left shift, but release both sides —
        // releasing an already-released key is a harmless no-op.
        for b in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            self.release(b)?;
        }
        self.key(KEY_LEFTSHIFT, false)?;
        self.key(KEY_RIGHTSHIFT, false)?;
        Ok(())
    }
}

// ---- window ---------------------------------------------------------------

/// The compositor runs one app fullscreen; window queries reflect that.
#[derive(Debug, Clone)]
pub struct VWaylandWindow {
    ipc: Ipc,
    width: f64,
    height: f64,
}

impl VWaylandWindow {
    pub fn new(sock_path: impl AsRef<Path>, width: u32, height: u32) -> Self {
        Self {
            ipc: Ipc {
                sock: sock_path.as_ref().to_path_buf(),
            },
            width: width as f64,
            height: height as f64,
        }
    }

    fn app_info(&mut self) -> Result<Option<WindowInfo>, ExecError> {
        let header = self.ipc.request(&serde_json::json!({"cmd": "ping"}))?;
        let pid = header.get("app_pid").and_then(serde_json::Value::as_u64);
        let Some(pid) = pid else {
            return Ok(None);
        };
        Ok(Some(WindowInfo {
            // Same contract as KWin: FNV-1a over the string id, so the
            // WindowId newtype stays honest about opacity.
            id: WindowId(fnv1a(&pid.to_string())),
            title: "vwayland app".to_string(),
            app_id: "vwayland".to_string(),
            focused: true,
            bounds: Some(WindowBounds {
                x: 0.0,
                y: 0.0,
                w: self.width,
                h: self.height,
            }),
        }))
    }
}

impl WindowBackend for VWaylandWindow {
    fn active_window(&mut self) -> Result<Option<WindowInfo>, ExecError> {
        self.app_info()
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>, ExecError> {
        Ok(self.app_info()?.into_iter().collect())
    }

    fn focus_window(&mut self, id: WindowId) -> Result<bool, ExecError> {
        // Single fullscreen app: the id either matches or nothing is
        // touched — the same "unknown id = false" contract as KWin.
        Ok(self.app_info()?.is_some_and(|w| w.id == id))
    }
}
