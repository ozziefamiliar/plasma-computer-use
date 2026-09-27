//! Absolute-uinput input backend.
//!
//! Three separate virtual devices, following the repo survey's hardest-won
//! lessons:
//!
//! - **True absolute pointer** (zetakai, measured 0px error at 2560x1440):
//!   `ABS_X`/`ABS_Y` with range `0..=65535`, matching the
//!   65535-normalization [`pcu_core::coord::UInputAbs`] the executor emits.
//!   `INPUT_PROP_DIRECT` so libinput treats it as a direct (absolute)
//!   pointer, not a touchpad. Acceleration-proof by construction.
//! - **Separate devices** for pointer+buttons, wheel, and keyboard —
//!   libinput classifies mixed devices unpredictably (survey, session #1).
//! - **Device-creation settle**: 350ms after `UI_DEV_CREATE` before the
//!   first event; the compositor discovers devices asynchronously (zetakai).
//!
//! Raw `/dev/uinput` ioctls via `libc` — no `evdev`/`uinput` crate, so this
//! crate keeps its tiny dependency footprint. The ioctl numbers are computed
//! from the `_IO`/`_IOW` macros and asserted against the known values in
//! tests, so a typo in the macro math fails loudly instead of misbehaving.
//!
//! LIVE-VALIDATION (Arch machine):
//! - absolute accuracy on fractional-scale multi-monitor (the zetakai
//!   5/5-at-0px claim needs re-measuring on KWin, not COSMIC);
//! - `wheel()` sign: evdev `REL_WHEEL` is up-positive, the trait contract is
//!   down-positive, and the survey notes KDE inverts the *continuous*
//!   vertical axis but not discrete — the final sign needs eyes on hardware.

use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::time::Duration;

use pcu_core::action::MouseButton;
use pcu_core::backend::InputBackend;
use pcu_core::coord::UInputAbs;
use pcu_core::result::ExecError;

// ---- ioctl numbers (linux/uinput.h) ----------------------------------------
// _IOW(type, nr, size) = (1<<30) | (size<<16) | (type<<8) | nr

const fn iow(nr: u32, size: u32) -> u64 {
    ((1 << 30) | (size << 16) | ((b'U' as u32) << 8) | nr) as u64
}

const UI_SET_EVBIT: u64 = iow(100, 4); // 0x40045564
const UI_SET_KEYBIT: u64 = iow(101, 4); // 0x40045565
const UI_SET_RELBIT: u64 = iow(102, 4); // 0x40045566
const UI_SET_ABSBIT: u64 = iow(103, 4); // 0x40045567
const UI_SET_PROPBIT: u64 = iow(110, 4); // 0x4004556e
const UI_DEV_CREATE: u64 = ((b'U' as u64) << 8) | 1; // _IO('U',1) = 0x5501
const UI_DEV_DESTROY: u64 = ((b'U' as u64) << 8) | 2; // _IO('U',2) = 0x5502
/// sizeof(struct uinput_setup) = 8 (input_id) + 80 (name) + 4 (ff) = 92.
const UI_DEV_SETUP: u64 = iow(3, 92); // 0x405c5503
/// sizeof(struct uinput_abs_setup) = 2 (code) + 2 (pad) + 24 (absinfo) = 28.
const UI_ABS_SETUP: u64 = iow(4, 28); // 0x401c5504

// ---- event codes (linux/input-event-codes.h) --------------------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const INPUT_PROP_DIRECT: u64 = 0x02;

/// Axis range for the absolute pointer: matches `UInputAbs`'s 65535
/// normalization in `pcu-core` (the zetakai recipe).
const AXIS_MAX: i32 = 65535;

// Key codes (linux/input-event-codes.h) used by the keyboard device.
const KEY_ESC: u16 = 1;
const KEY_1: u16 = 2;
const KEY_2: u16 = 3;
const KEY_3: u16 = 4;
const KEY_4: u16 = 5;
const KEY_5: u16 = 6;
const KEY_6: u16 = 7;
const KEY_7: u16 = 8;
const KEY_8: u16 = 9;
const KEY_9: u16 = 10;
const KEY_0: u16 = 11;
const KEY_MINUS: u16 = 12;
const KEY_EQUAL: u16 = 13;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_Q: u16 = 16;
const KEY_W: u16 = 17;
const KEY_E: u16 = 18;
const KEY_R: u16 = 19;
const KEY_T: u16 = 20;
const KEY_Y: u16 = 21;
const KEY_U: u16 = 22;
const KEY_I: u16 = 23;
const KEY_O: u16 = 24;
const KEY_P: u16 = 25;
const KEY_LEFTBRACE: u16 = 26;
const KEY_RIGHTBRACE: u16 = 27;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_A: u16 = 30;
const KEY_S: u16 = 31;
const KEY_D: u16 = 32;
const KEY_F: u16 = 33;
const KEY_G: u16 = 34;
const KEY_H: u16 = 35;
const KEY_J: u16 = 36;
const KEY_K: u16 = 37;
const KEY_L: u16 = 38;
const KEY_SEMICOLON: u16 = 39;
const KEY_APOSTROPHE: u16 = 40;
const KEY_GRAVE: u16 = 41;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_BACKSLASH: u16 = 43;
const KEY_Z: u16 = 44;
const KEY_X: u16 = 45;
const KEY_C: u16 = 46;
const KEY_V: u16 = 47;
const KEY_B: u16 = 48;
const KEY_N: u16 = 49;
const KEY_M: u16 = 50;
const KEY_COMMA: u16 = 51;
const KEY_DOT: u16 = 52;
const KEY_SLASH: u16 = 53;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_SPACE: u16 = 57;
const KEY_F1: u16 = 59;
const KEY_F2: u16 = 60;
const KEY_F3: u16 = 61;
const KEY_F4: u16 = 62;
const KEY_F5: u16 = 63;
const KEY_F6: u16 = 64;
const KEY_F7: u16 = 65;
const KEY_F8: u16 = 66;
const KEY_F9: u16 = 67;
const KEY_F10: u16 = 68;
const KEY_F11: u16 = 87;
const KEY_F12: u16 = 88;
const KEY_HOME: u16 = 102;
const KEY_UP: u16 = 103;
const KEY_PAGEUP: u16 = 104;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_END: u16 = 107;
const KEY_DOWN: u16 = 108;
const KEY_PAGEDOWN: u16 = 109;
const KEY_INSERT: u16 = 110;
const KEY_DELETE: u16 = 111;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
// Right-side modifiers have no key_code() name yet, but `release_all`
// needs them (a stuck chord could hold either side).
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;

/// Every key the keyboard device registers. `keypress`/`type_text` can only
/// ever emit codes from this table; unknown names fail as `Backend` errors
/// (never silently dropped).
const ALL_KEYS: &[u16] = &[
    KEY_ESC, KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9, KEY_0, KEY_MINUS,
    KEY_EQUAL, KEY_BACKSPACE, KEY_TAB, KEY_Q, KEY_W, KEY_E, KEY_R, KEY_T, KEY_Y, KEY_U, KEY_I,
    KEY_O, KEY_P, KEY_LEFTBRACE, KEY_RIGHTBRACE, KEY_ENTER, KEY_LEFTCTRL, KEY_A, KEY_S, KEY_D,
    KEY_F, KEY_G, KEY_H, KEY_J, KEY_K, KEY_L, KEY_SEMICOLON, KEY_APOSTROPHE, KEY_GRAVE,
    KEY_LEFTSHIFT, KEY_BACKSLASH, KEY_Z, KEY_X, KEY_C, KEY_V, KEY_B, KEY_N, KEY_M, KEY_COMMA,
    KEY_DOT, KEY_SLASH, KEY_RIGHTSHIFT, KEY_LEFTALT, KEY_RIGHTALT, KEY_SPACE, KEY_F1, KEY_F2, KEY_F3, KEY_F4,
    KEY_F5, KEY_F6, KEY_F7, KEY_F8, KEY_F9, KEY_F10, KEY_F11, KEY_F12, KEY_HOME, KEY_UP,
    KEY_PAGEUP, KEY_LEFT, KEY_RIGHT, KEY_END, KEY_DOWN, KEY_PAGEDOWN, KEY_INSERT, KEY_DELETE,
    KEY_RIGHTCTRL, KEY_LEFTMETA, KEY_RIGHTMETA,
];

// ---- repr(C) structs (linux/input.h, linux/uinput.h) ------------------------

/// `struct uinput_setup`: input_id (8) + name[80] + ff_effects_max (4) = 92.
#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

/// `struct uinput_abs_setup`: code (2) + pad (2) + input_absinfo (24) = 28.
#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// `struct input_event` on 64-bit Linux: timeval (16) + type/code (4) +
/// value (4) = 24 bytes.
#[repr(C)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

impl InputEvent {
    fn bytes(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&self.tv_sec.to_ne_bytes());
        b[8..16].copy_from_slice(&self.tv_usec.to_ne_bytes());
        b[16..18].copy_from_slice(&self.type_.to_ne_bytes());
        b[18..20].copy_from_slice(&self.code.to_ne_bytes());
        b[20..24].copy_from_slice(&self.value.to_ne_bytes());
        b
    }
}

// ---- raw device --------------------------------------------------------------

/// One `/dev/uinput` virtual device. `Drop` issues `UI_DEV_DESTROY` before
/// the fd closes.
struct RawDevice {
    file: File,
    created: bool,
    name: &'static str,
}

impl RawDevice {
    fn open() -> Result<RawFd, ExecError> {
        let path = CString::new("/dev/uinput").unwrap();
        // O_WRONLY | O_NONBLOCK, mode 0 (device node already exists).
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK, 0) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(ExecError::Infra(format!(
                "cannot open /dev/uinput ({e}); need read/write access \
                 (usually the `input` group or root)"
            )));
        }
        Ok(fd)
    }

    fn ioctl_set(fd: RawFd, req: u64, bit: u64, what: &str) -> Result<(), ExecError> {
        let r = unsafe { libc::ioctl(fd, req as libc::c_ulong, bit as libc::c_ulong) };
        if r < 0 {
            return Err(ExecError::Infra(format!(
                "uinput ioctl failed ({what}): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    fn ioctl_ptr<T>(fd: RawFd, req: u64, arg: *const T, what: &str) -> Result<(), ExecError> {
        let r = unsafe { libc::ioctl(fd, req as libc::c_ulong, arg) };
        if r < 0 {
            return Err(ExecError::Infra(format!(
                "uinput ioctl failed ({what}): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Build a device: set the event/key/rel/abs/prop bits, axis ranges,
    /// then `UI_DEV_SETUP` + `UI_DEV_CREATE` + the 350ms discovery settle.
    /// `direct_prop` sets `INPUT_PROP_DIRECT` (pointer only — libinput
    /// classifies mixed devices unpredictably, so wheel/keyboard skip it).
    fn create(
        name: &'static str,
        evbits: &[u16],
        keybits: &[u16],
        relbits: &[u16],
        absbits: &[u16],
        abs_ranges: &[(u16, i32, i32)],
        direct_prop: bool,
    ) -> Result<Self, ExecError> {
        let fd = Self::open()?;
        // SAFETY: fd is a fresh, owned open file description.
        let file = unsafe { File::from_raw_fd(fd) };
        let raw = file.as_raw_fd();
        for &ev in evbits {
            Self::ioctl_set(raw, UI_SET_EVBIT, ev as u64, "UI_SET_EVBIT")?;
        }
        for &k in keybits {
            Self::ioctl_set(raw, UI_SET_KEYBIT, k as u64, "UI_SET_KEYBIT")?;
        }
        for &r in relbits {
            Self::ioctl_set(raw, UI_SET_RELBIT, r as u64, "UI_SET_RELBIT")?;
        }
        for &a in absbits {
            Self::ioctl_set(raw, UI_SET_ABSBIT, a as u64, "UI_SET_ABSBIT")?;
        }
        if direct_prop {
            Self::ioctl_set(raw, UI_SET_PROPBIT, INPUT_PROP_DIRECT, "UI_SET_PROPBIT")?;
        }
        for &(code, min, max) in abs_ranges {
            let abs = UinputAbsSetup {
                code,
                _pad: 0,
                value: 0,
                minimum: min,
                maximum: max,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            };
            Self::ioctl_ptr(raw, UI_ABS_SETUP, &abs, "UI_ABS_SETUP")?;
        }
        let mut name_buf = [0u8; 80];
        let n = name.as_bytes();
        name_buf[..n.len().min(79)].copy_from_slice(&n[..n.len().min(79)]);
        let dev = UinputSetup {
            bustype: 0x03,
            vendor: 0x9d1,
            product: 0x01,
            version: 1,
            name: name_buf,
            ff_effects_max: 0,
        };
        Self::ioctl_ptr(raw, UI_DEV_SETUP, &dev, "UI_DEV_SETUP")?;
        let r = unsafe { libc::ioctl(raw, UI_DEV_CREATE as libc::c_ulong) };
        if r < 0 {
            return Err(ExecError::Infra(format!(
                "UI_DEV_CREATE failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        // The compositor discovers virtual devices asynchronously; zetakai
        // measured 350ms before the first event is honored.
        std::thread::sleep(Duration::from_millis(350));
        Ok(Self {
            file,
            created: true,
            name,
        })
    }

    fn emit(&mut self, events: &[(u16, u16, i32)]) -> Result<(), ExecError> {
        for &(type_, code, value) in events {
            let ev = InputEvent {
                tv_sec: 0,
                tv_usec: 0,
                type_,
                code,
                value,
            };
            self.file.write_all(&ev.bytes()).map_err(|e| {
                ExecError::Infra(format!("uinput write failed on {}: {e}", self.name))
            })?;
        }
        // SYN_REPORT terminates every emission.
        let syn = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };
        self.file.write_all(&syn.bytes()).map_err(|e| {
            ExecError::Infra(format!("uinput write failed on {}: {e}", self.name))
        })?;
        Ok(())
    }
}

impl Drop for RawDevice {
    fn drop(&mut self) {
        if self.created {
            unsafe {
                libc::ioctl(
                    self.file.as_raw_fd(),
                    UI_DEV_DESTROY as libc::c_ulong,
                );
            }
        }
        // `file` closes the fd here.
    }
}

// ---- key name resolution -----------------------------------------------------

/// Letter → evdev key code. evdev codes run left-to-right along each
/// QWERTY keyboard row (QWERTYUIOP → 16..=25, ASDFGHJKL → 30..=38,
/// ZXCVBNM → 44..=50), NOT per alphabet — `KEY_A + (c-'A')` would send the
/// wrong code for every letter outside the home row. The table spells each
/// row out positionally so the mapping is auditable at a glance.
fn letter_key(c: char) -> Option<u16> {
    const ROWS: &[(char, u16)] = &[
        ('Q', KEY_Q), ('W', KEY_W), ('E', KEY_E), ('R', KEY_R), ('T', KEY_T),
        ('Y', KEY_Y), ('U', KEY_U), ('I', KEY_I), ('O', KEY_O), ('P', KEY_P),
        ('A', KEY_A), ('S', KEY_S), ('D', KEY_D), ('F', KEY_F), ('G', KEY_G),
        ('H', KEY_H), ('J', KEY_J), ('K', KEY_K), ('L', KEY_L),
        ('Z', KEY_Z), ('X', KEY_X), ('C', KEY_C), ('V', KEY_V), ('B', KEY_B),
        ('N', KEY_N), ('M', KEY_M),
    ];
    ROWS.iter().find(|(ch, _)| *ch == c).map(|(_, k)| *k)
}

/// Resolve a `keypress` key name (case-insensitive) to an evdev key code.
/// Returns `None` for unknown names — the caller turns that into a
/// `Backend` error, never a silent no-op.
fn key_code(name: &str) -> Option<u16> {
    let n = name.trim().to_ascii_uppercase();
    if n.len() == 1 {
        let c = n.chars().next().unwrap();
        return match c {
            'A'..='Z' => letter_key(c),
            '0'..='9' => {
                Some(if c == '0' {
                    KEY_0
                } else {
                    KEY_1 + (c as u16 - '1' as u16)
                })
            }
            ' ' => Some(KEY_SPACE),
            _ => char_key(c),
        };
    }
    Some(match n.as_str() {
        "ESC" | "ESCAPE" => KEY_ESC,
        "TAB" => KEY_TAB,
        "ENTER" | "RETURN" => KEY_ENTER,
        "SPACE" => KEY_SPACE,
        "BACKSPACE" | "BACK" => KEY_BACKSPACE,
        "DELETE" | "DEL" => KEY_DELETE,
        "INSERT" | "INS" => KEY_INSERT,
        "HOME" => KEY_HOME,
        "END" => KEY_END,
        "PAGEUP" | "PGUP" | "PRIOR" => KEY_PAGEUP,
        "PAGEDOWN" | "PGDN" | "NEXT" => KEY_PAGEDOWN,
        "UP" => KEY_UP,
        "DOWN" => KEY_DOWN,
        "LEFT" => KEY_LEFT,
        "RIGHT" => KEY_RIGHT,
        "CTRL" | "CONTROL" | "LEFTCTRL" => KEY_LEFTCTRL,
        "SHIFT" | "LEFTSHIFT" => KEY_LEFTSHIFT,
        "RIGHTSHIFT" => KEY_RIGHTSHIFT,
        "ALT" | "LEFTALT" => KEY_LEFTALT,
        "META" | "SUPER" | "WIN" | "LEFTMETA" => KEY_LEFTMETA,
        "RIGHTMETA" => KEY_RIGHTMETA,
        "MINUS" | "DASH" => KEY_MINUS,
        "EQUAL" | "EQUALS" => KEY_EQUAL,
        "SEMICOLON" => KEY_SEMICOLON,
        "APOSTROPHE" | "QUOTE" => KEY_APOSTROPHE,
        "GRAVE" | "BACKTICK" => KEY_GRAVE,
        "BACKSLASH" => KEY_BACKSLASH,
        "COMMA" => KEY_COMMA,
        "DOT" | "PERIOD" => KEY_DOT,
        "SLASH" => KEY_SLASH,
        "LEFTBRACE" | "LBRACKET" => KEY_LEFTBRACE,
        "RIGHTBRACE" | "RBRACKET" => KEY_RIGHTBRACE,
        "F1" => KEY_F1,
        "F2" => KEY_F2,
        "F3" => KEY_F3,
        "F4" => KEY_F4,
        "F5" => KEY_F5,
        "F6" => KEY_F6,
        "F7" => KEY_F7,
        "F8" => KEY_F8,
        "F9" => KEY_F9,
        "F10" => KEY_F10,
        "F11" => KEY_F11,
        "F12" => KEY_F12,
        _ => return None,
    })
}

/// Single non-alphanumeric printable char → key code (US layout positions).
fn char_key(c: char) -> Option<u16> {
    Some(match c {
        '-' | '_' => KEY_MINUS,
        '=' | '+' => KEY_EQUAL,
        '[' | '{' => KEY_LEFTBRACE,
        ']' | '}' => KEY_RIGHTBRACE,
        '\\' | '|' => KEY_BACKSLASH,
        ';' | ':' => KEY_SEMICOLON,
        '\'' | '"' => KEY_APOSTROPHE,
        '`' | '~' => KEY_GRAVE,
        ',' | '<' => KEY_COMMA,
        '.' | '>' => KEY_DOT,
        '/' | '?' => KEY_SLASH,
        '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')' => {
            // Shifted digits: handled by the shifted branch of `us_key`
            // before this function is ever consulted.
            return None;
        }
        _ => return None,
    })
}

/// Printable ASCII char → (key code, needs_shift), US layout. `None` means
/// "not typeable through raw keycodes" — the caller must not silently drop
/// it (see `type_text`).
fn us_key(c: char) -> Option<(u16, bool)> {
    if c == ' ' {
        return Some((KEY_SPACE, false));
    }
    if c.is_ascii_alphabetic() {
        let code = letter_key(c.to_ascii_uppercase())?;
        return Some((code, c.is_ascii_uppercase()));
    }
    if c.is_ascii_digit() {
        let code = if c == '0' {
            KEY_0
        } else {
            KEY_1 + (c as u16 - '1' as u16)
        };
        return Some((code, false));
    }
    let shifted = "!@#$%^&*()_+{}|:\"<>?~";
    if shifted.contains(c) {
        let base = match c {
            '!' => '1',
            '@' => '2',
            '#' => '3',
            '$' => '4',
            '%' => '5',
            '^' => '6',
            '&' => '7',
            '*' => '8',
            '(' => '9',
            ')' => '0',
            '_' => '-',
            '+' => '=',
            '{' => '[',
            '}' => ']',
            '|' => '\\',
            ':' => ';',
            '"' => '\'',
            '<' => ',',
            '>' => '.',
            '?' => '/',
            '~' => '`',
            _ => return None,
        };
        let (code, _) = us_key(base)?;
        return Some((code, true));
    }
    char_key(c).map(|code| (code, false))
}

// ---- the backend ---------------------------------------------------------------

/// Absolute-uinput input backend: three virtual devices (pointer, wheel,
/// keyboard) implementing [`InputBackend`].
///
/// The executor owns sequencing and timing; this backend only emits the raw
/// events it is told to. Coordinates arrive 65535-normalized
/// ([`UInputAbs`]) and are emitted verbatim on the `0..=65535` axes.
pub struct UInputBackend {
    pointer: RawDevice,
    wheel: RawDevice,
    keyboard: RawDevice,
}

impl UInputBackend {
    /// Create the three virtual devices. Fails with `Infra` (naming the
    /// cause) when `/dev/uinput` isn't available or writable.
    pub fn try_new() -> Result<Self, ExecError> {
        let pointer = RawDevice::create(
            "pcu-pointer",
            &[EV_SYN, EV_KEY, EV_ABS],
            &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
            &[],
            &[ABS_X, ABS_Y],
            &[(ABS_X, 0, AXIS_MAX), (ABS_Y, 0, AXIS_MAX)],
            true, // INPUT_PROP_DIRECT: absolute, not a touchpad
        )?;
        let wheel = RawDevice::create(
            "pcu-wheel",
            &[EV_SYN, EV_REL],
            &[],
            &[REL_WHEEL, REL_HWHEEL],
            &[],
            &[],
            false,
        )?;
        let keyboard = RawDevice::create(
            "pcu-keyboard",
            &[EV_SYN, EV_KEY],
            ALL_KEYS,
            &[],
            &[],
            &[],
            false,
        )?;
        Ok(Self {
            pointer,
            wheel,
            keyboard,
        })
    }

    fn button_code(b: MouseButton) -> u16 {
        match b {
            MouseButton::Left => BTN_LEFT,
            MouseButton::Right => BTN_RIGHT,
            MouseButton::Middle => BTN_MIDDLE,
        }
    }

    fn key_event(&mut self, code: u16, down: bool) -> Result<(), ExecError> {
        self.keyboard
            .emit(&[(EV_KEY, code, if down { 1 } else { 0 })])
    }

    /// Type ASCII text through raw keycodes (US layout). Used when `wtype`
    /// isn't installed. Any char outside printable ASCII → `Backend` error
    /// naming the char: never silently dropped.
    fn type_ascii(&mut self, text: &str) -> Result<(), ExecError> {
        for c in text.chars() {
            let (code, shift) = us_key(c).ok_or_else(|| {
                ExecError::Backend(format!(
                    "cannot type {c:?} (U+{:04X}) through raw keycodes; \
                     install `wtype` for layout-independent typing, or use \
                     clipboard paste (not yet implemented)",
                    c as u32
                ))
            })?;
            if shift {
                self.key_event(KEY_LEFTSHIFT, true)?;
            }
            self.key_event(code, true)?;
            self.key_event(code, false)?;
            if shift {
                self.key_event(KEY_LEFTSHIFT, false)?;
            }
        }
        Ok(())
    }
}

impl InputBackend for UInputBackend {
    fn move_to(&mut self, p: UInputAbs) -> Result<(), ExecError> {
        self.pointer.emit(&[
            (EV_ABS, ABS_X, p.x as i32),
            (EV_ABS, ABS_Y, p.y as i32),
        ])
    }

    fn press(&mut self, button: MouseButton) -> Result<(), ExecError> {
        let code = Self::button_code(button);
        self.pointer.emit(&[(EV_KEY, code, 1)])
    }

    fn release(&mut self, button: MouseButton) -> Result<(), ExecError> {
        let code = Self::button_code(button);
        self.pointer.emit(&[(EV_KEY, code, 0)])
    }

    fn release_all(&mut self) -> Result<(), ExecError> {
        // Buttons first, then modifiers: a stuck drag holds Left; a stuck
        // chord (`keypress`/`type_text` failing mid-sequence) holds
        // modifiers. Releasing an already-released key is a harmless no-op
        // on evdev, so enumerate everything rather than track state. Both
        // sides of each modifier are covered (they're all in ALL_KEYS, so
        // the device accepts the events).
        for b in [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
        ] {
            self.release(b)?;
        }
        for code in [
            KEY_LEFTCTRL,
            KEY_RIGHTCTRL,
            KEY_LEFTSHIFT,
            KEY_RIGHTSHIFT,
            KEY_LEFTALT,
            KEY_RIGHTALT,
            KEY_LEFTMETA,
            KEY_RIGHTMETA,
        ] {
            self.key_event(code, false)?;
        }
        Ok(())
    }

    fn wheel(&mut self, dx: f64, dy: f64) -> Result<(), ExecError> {
        // Trait contract: positive = right/down. evdev REL_WHEEL is
        // up-positive, so negate. (Whether KWin/libinput honors the sign on
        // a virtual device — and the survey's KDE discrete-axis note — is a
        // live-validation item; the conversion itself is kernel ABI.)
        let up = -(dy.round() as i32);
        let right = dx.round() as i32;
        self.wheel.emit(&[(EV_REL, REL_WHEEL, up), (EV_REL, REL_HWHEEL, right)])
    }

    fn keypress(&mut self, keys: &[String]) -> Result<(), ExecError> {
        let codes: Vec<u16> = keys
            .iter()
            .map(|k| {
                key_code(k).ok_or_else(|| {
                    ExecError::Backend(format!("unknown key name: {k:?}"))
                })
            })
            .collect::<Result<_, _>>()?;
        // Resolve everything *before* touching the device: no partial chords.
        for &c in &codes {
            self.key_event(c, true)?;
        }
        for &c in codes.iter().rev() {
            self.key_event(c, false)?;
        }
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), ExecError> {
        // Primary path (survey, session #1): `wtype` types keysyms,
        // layout-independent — handles héllo 🐺 where raw keycodes can't.
        match std::process::Command::new("wtype")
            .arg("--")
            .arg(text)
            .output()
        {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(ExecError::Backend(format!(
                "wtype failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Fallback: raw keycodes, ASCII only, honest about the rest.
                self.type_ascii(text)
            }
            Err(e) => Err(ExecError::Infra(format!("failed to spawn wtype: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_linux_uinput_h() {
        // If the const-fn macro math is wrong, every device setup fails —
        // assert the well-known values, not just internal consistency.
        assert_eq!(UI_SET_EVBIT, 0x40045564);
        assert_eq!(UI_SET_KEYBIT, 0x40045565);
        assert_eq!(UI_SET_RELBIT, 0x40045566);
        assert_eq!(UI_SET_ABSBIT, 0x40045567);
        assert_eq!(UI_SET_PROPBIT, 0x4004556e);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_DEV_SETUP, 0x405c5503);
        assert_eq!(UI_ABS_SETUP, 0x401c5504);
    }

    #[test]
    fn repr_c_struct_sizes_match_kernel_layout() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
    }

    #[test]
    fn axis_range_matches_core_uinputabs_normalization() {
        // core maps the desktop to 0..=65535; the device must accept all of it.
        assert_eq!(AXIS_MAX, 65535);
        assert_eq!(u16::MAX as i32, AXIS_MAX);
    }

    #[test]
    fn key_names_resolve() {
        assert_eq!(key_code("a"), Some(KEY_A));
        assert_eq!(key_code("Z"), Some(KEY_Z));
        assert_eq!(key_code("5"), Some(KEY_5));
        assert_eq!(key_code("0"), Some(KEY_0));
        assert_eq!(key_code("ctrl"), Some(KEY_LEFTCTRL));
        assert_eq!(key_code("CONTROL"), Some(KEY_LEFTCTRL));
        assert_eq!(key_code("Shift"), Some(KEY_LEFTSHIFT));
        assert_eq!(key_code("Alt"), Some(KEY_LEFTALT));
        assert_eq!(key_code("meta"), Some(KEY_LEFTMETA));
        assert_eq!(key_code("super"), Some(KEY_LEFTMETA));
        assert_eq!(key_code("Escape"), Some(KEY_ESC));
        assert_eq!(key_code("Return"), Some(KEY_ENTER));
        assert_eq!(key_code("Backspace"), Some(KEY_BACKSPACE));
        assert_eq!(key_code("Delete"), Some(KEY_DELETE));
        assert_eq!(key_code("Prior"), Some(KEY_PAGEUP));
        assert_eq!(key_code("F12"), Some(KEY_F12));
        assert_eq!(key_code("F13"), None);
        assert_eq!(key_code("side"), None);
        assert_eq!(key_code(""), None);
        // evdev codes follow keyboard rows, not the alphabet: every row
        // endpoint, both ends, so a per-alphabet regression fails loudly.
        assert_eq!(key_code("q"), Some(KEY_Q));
        assert_eq!(key_code("P"), Some(KEY_P));
        assert_eq!(key_code("w"), Some(KEY_W)); // 17, not KEY_A+22
        assert_eq!(key_code("l"), Some(KEY_L));
        assert_eq!(key_code("b"), Some(KEY_B)); // 48, not KEY_A+1
        assert_eq!(key_code("m"), Some(KEY_M));
        assert_eq!(key_code("v"), Some(KEY_V));
    }

    #[test]
    fn us_key_shift_map() {
        assert_eq!(us_key('a'), Some((KEY_A, false)));
        assert_eq!(us_key('A'), Some((KEY_A, true)));
        assert_eq!(us_key('!'), Some((KEY_1, true)));
        assert_eq!(us_key('?'), Some((KEY_SLASH, true)));
        assert_eq!(us_key(' '), Some((KEY_SPACE, false)));
        assert_eq!(us_key('é'), None); // honest: not typeable via raw keycodes
        assert_eq!(us_key('🐺'), None);
        // row-wise codes, not alphabetic arithmetic
        assert_eq!(us_key('z'), Some((KEY_Z, false)));
        assert_eq!(us_key('Z'), Some((KEY_Z, true)));
        assert_eq!(us_key('q'), Some((KEY_Q, false)));
        assert_eq!(us_key('W'), Some((KEY_W, true)));
        assert_eq!(us_key('L'), Some((KEY_L, true)));
    }

    #[test]
    fn all_registered_keys_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for &k in ALL_KEYS {
            assert!(seen.insert(k), "duplicate key code {k}");
        }
        // The keypress table must only reference registered codes.
        for name in ["a", "ctrl", "F5", "Home", "space", "Left"] {
            let c = key_code(name).unwrap();
            assert!(seen.contains(&c), "{name} not registered on the device");
        }
    }

    #[test]
    fn input_event_wire_layout() {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_: EV_ABS,
            code: ABS_X,
            value: 32768,
        };
        let b = ev.bytes();
        assert_eq!(b.len(), 24);
        assert_eq!(u16::from_ne_bytes([b[16], b[17]]), EV_ABS);
        assert_eq!(u16::from_ne_bytes([b[18], b[19]]), ABS_X);
        assert_eq!(i32::from_ne_bytes([b[20], b[21], b[22], b[23]]), 32768);
    }
}
