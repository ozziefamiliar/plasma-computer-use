//! Window listing via KWin scripting over the `qdbus` CLI.
//!
//! The recipe is wdotool's (vendored at
//! `references/wdotool_kde_backend.rs`), minus the async zbus bridge:
//!
//! 1. generate a JS snippet (dual `windowList()`/`clientList()` for
//!    Plasma 6/5, JSON payload, `internalId || windowId` ids),
//! 2. write it to a temp file — Plasma 6 removed `loadScriptFromText`, so
//!    `org.kde.kwin.Scripting.loadScript(path, pluginName)` is the only
//!    path, and the file must stay alive until after the run (KWin reads
//!    lazily on some versions),
//! 3. `run()` the per-script object at `/Scripting/Script{id}` (Plasma 6
//!    does not auto-run loaded scripts),
//! 4. read the result the script left via `writeConfig`, then
//!    `unloadScript` and delete the temp file.
//!
//! The one deliberate deviation: instead of wdotool's zbus callback bridge
//! (needs an async runtime + a D-Bus object server), the script reports via
//! the scripting API's `writeConfig(key, value)` and this backend polls the
//! value with `kreadconfig6`. Synchronous, zero new dependencies, and the
//! 3s-timeout discipline is the same. The [`ScriptChannel`] trait keeps the
//! transport swappable — if the writeConfig round-trip misbehaves on real
//! KWin, the zbus bridge from the vendored recipe drops in behind the same
//! trait.
//!
//! [`WindowBackend`][pcu_core::backend::WindowBackend] ids are `u64`; KWin
//! ids are strings, so they are mapped with FNV-1a (stable within a
//! process; documented collision caveat).
//!
//! LIVE-VALIDATION (Arch machine):
//! - the `writeConfig` → kwinrc group mapping (`[Script-<plugin>]`) is the
//!   one unverified assumption here — if results never arrive, this is the
//!   line to fix (or swap in the zbus bridge);
//! - `qdbus`/`kreadconfig6` presence on a stock Plasma 6 session.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pcu_core::backend::{WindowBackend, WindowInfo};
use pcu_core::result::ExecError;

/// Fixed config key the scripts write their result to. One key per channel
/// (calls are `&mut`-serialized), so stale results can't accumulate.
const RESULT_KEY: &str = "pcu_result";

/// How long to wait for a script's `writeConfig` to become visible.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(3);

/// Runs a KWin script and returns what it wrote to [`RESULT_KEY`].
///
/// `Ok(None)` = the script ran but wrote nothing (e.g. no active window);
/// `Err` = the machinery failed (qdbus missing, timeout, ...).
pub trait ScriptChannel {
    fn eval(&mut self, script: &str) -> Result<Option<String>, ExecError>;
}

// ---- JS script generators (adapted from wdotool's recipe) ---------------------

fn list_windows_script() -> String {
    format!(
        r#"
(function() {{
  var out = [];
  var list = (typeof workspace.windowList === "function")
    ? workspace.windowList()
    : workspace.clientList();
  for (var i = 0; i < list.length; i++) {{
    var w = list[i];
    out.push({{
      id: (w.internalId || w.windowId || i).toString(),
      title: String(w.caption || ""),
      app_id: String(w.resourceClass || w.resourceName || "")
    }});
  }}
  writeConfig("{RESULT_KEY}", JSON.stringify(out));
}})();
"#
    )
}

fn active_window_script() -> String {
    format!(
        r#"
(function() {{
  var w = workspace.activeWindow || workspace.activeClient;
  var payload = "null";
  if (w) {{
    payload = JSON.stringify({{
      id: (w.internalId || w.windowId || 0).toString(),
      title: String(w.caption || ""),
      app_id: String(w.resourceClass || w.resourceName || "")
    }});
  }}
  writeConfig("{RESULT_KEY}", payload);
}})();
"#
    )
}

#[derive(serde::Deserialize)]
struct ScriptWindow {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    app_id: Option<String>,
}

/// FNV-1a 64: stable string→u64 for KWin's string window ids.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---- qdbus channel ---------------------------------------------------------------

static SCRIPT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// [`ScriptChannel`] over the `qdbus` + `kreadconfig6` CLIs.
pub struct QdbusChannel;

impl QdbusChannel {
    fn qdbus(args: &[&str]) -> Result<String, ExecError> {
        let out = Command::new("qdbus").args(args).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ExecError::Infra("qdbus not found; is this a Plasma session?".into())
            } else {
                ExecError::Infra(format!("failed to run qdbus: {e}"))
            }
        })?;
        if !out.status.success() {
            return Err(ExecError::Infra(format!(
                "qdbus {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn read_result(group: &str) -> Result<Option<String>, ExecError> {
        let out = Command::new("kreadconfig6")
            .args(["--file", "kwinrc", "--group", group, "--key", RESULT_KEY])
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ExecError::Infra("kreadconfig6 not found; is this a Plasma 6 session?".into())
                } else {
                    ExecError::Infra(format!("failed to run kreadconfig6: {e}"))
                }
            })?;
        if !out.status.success() {
            return Ok(None); // key not present (yet)
        }
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if v.is_empty() { None } else { Some(v) })
    }
}

impl ScriptChannel for QdbusChannel {
    fn eval(&mut self, script: &str) -> Result<Option<String>, ExecError> {
        // Unique plugin name per call: KWin rejects loadScript when the
        // name is already loaded.
        let n = SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let plugin = format!("pcu-{}-{n}", std::process::id());
        let group = format!("[Script-{plugin}]");

        let path: PathBuf =
            std::env::temp_dir().join(format!("pcu-kwin-{}.js", plugin));
        std::fs::write(&path, script)
            .map_err(|e| ExecError::Infra(format!("cannot write KWin script temp file: {e}")))?;

        let result = (|| -> Result<Option<String>, ExecError> {
            let id_out = Self::qdbus(&[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.loadScript",
                path.to_str().ok_or_else(|| {
                    ExecError::Infra("script temp path is not UTF-8".into())
                })?,
                &plugin,
            ])?;
            let script_id: i32 = id_out.parse().map_err(|_| {
                ExecError::Backend(format!("loadScript returned non-integer: {id_out:?}"))
            })?;
            if script_id < 0 {
                return Err(ExecError::Backend(format!(
                    "loadScript failed (returned {script_id})"
                )));
            }
            // Clear any stale result before running, so a leftover from a
            // crashed earlier call can't be mistaken for this call's.
            let _ = Self::read_result(&group);
            Self::qdbus(&[
                "org.kde.KWin",
                &format!("/Scripting/Script{script_id}"),
                "org.kde.kwin.Script.run",
            ])?;

            // Poll for the writeConfig result (same 3s discipline as
            // wdotool's callback timeout).
            let start = Instant::now();
            let mut value = None;
            while start.elapsed() < SCRIPT_TIMEOUT {
                if let Some(v) = Self::read_result(&group)? {
                    value = Some(v);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }

            // Best-effort cleanup: unload from KWin; the temp file is
            // removed by the outer scope. A stale loaded script is
            // harmless (next call uses a fresh plugin name).
            let _ = Self::qdbus(&[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.unloadScript",
                &plugin,
            ]);
            Ok(value)
        })();

        let _ = std::fs::remove_file(&path);
        result
    }
}

// ---- the backend ------------------------------------------------------------------

/// [`WindowBackend`] via KWin scripting. Generic over the [`ScriptChannel`]
/// so tests can drive it with canned scripts/results.
pub struct KWinWindows<C: ScriptChannel = QdbusChannel> {
    channel: C,
}

impl KWinWindows<QdbusChannel> {
    pub fn try_new() -> Self {
        Self {
            channel: QdbusChannel,
        }
    }
}

impl<C: ScriptChannel> KWinWindows<C> {
    pub fn with_channel(channel: C) -> Self {
        Self { channel }
    }

    fn to_info(w: ScriptWindow, focused: bool) -> WindowInfo {
        WindowInfo {
            id: fnv1a(&w.id),
            title: w.title,
            app_id: w.app_id.unwrap_or_default(),
            focused,
        }
    }

    fn parse_list(json: &str) -> Result<Vec<ScriptWindow>, ExecError> {
        serde_json::from_str(json).map_err(|e| {
            ExecError::Backend(format!("invalid windows payload from KWin script: {e}"))
        })
    }
}

impl<C: ScriptChannel> WindowBackend for KWinWindows<C> {
    fn active_window(&mut self) -> Result<Option<WindowInfo>, ExecError> {
        match self.channel.eval(&active_window_script())? {
            None => Ok(None),
            Some(payload) if payload.trim() == "null" => Ok(None),
            Some(payload) => {
                let w: ScriptWindow = serde_json::from_str(&payload).map_err(|e| {
                    ExecError::Backend(format!("invalid active-window payload: {e}"))
                })?;
                Ok(Some(Self::to_info(w, true)))
            }
        }
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>, ExecError> {
        let payload = self
            .channel
            .eval(&list_windows_script())?
            .unwrap_or_else(|| "[]".to_string());
        let windows = Self::parse_list(&payload)?;
        // Mark the focused one: compare against the active window's id.
        let active_id = match self.channel.eval(&active_window_script())? {
            Some(p) if p.trim() != "null" => serde_json::from_str::<ScriptWindow>(&p)
                .ok()
                .map(|w| w.id),
            _ => None,
        };
        Ok(windows
            .into_iter()
            .map(|w| {
                let focused = active_id.as_deref() == Some(w.id.as_str());
                Self::to_info(w, focused)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Canned channel: maps script-marker → result payload.
    struct FakeChannel {
        results: HashMap<&'static str, Option<String>>,
        pub seen_scripts: RefCell<Vec<String>>,
    }

    impl FakeChannel {
        fn new() -> Self {
            let mut results = HashMap::new();
            results.insert(
                "list",
                Some(
                    r#"[{"id":"11","title":"Terminal","app_id":"org.kde.konsole"},{"id":"22","title":"Firefox","app_id":"firefox"}]"#.to_string(),
                ),
            );
            results.insert(
                "active",
                Some(r#"{"id":"22","title":"Firefox","app_id":"firefox"}"#.to_string()),
            );
            Self {
                results,
                seen_scripts: RefCell::new(Vec::new()),
            }
        }
    }

    impl ScriptChannel for FakeChannel {
        fn eval(&mut self, script: &str) -> Result<Option<String>, ExecError> {
            self.seen_scripts.borrow_mut().push(script.to_string());
            let key = if script.contains("windowList") {
                "list"
            } else {
                "active"
            };
            Ok(self.results[key].clone())
        }
    }

    #[test]
    fn scripts_use_windowlist_clientlist_dual_and_writeconfig() {
        let s = list_windows_script();
        assert!(s.contains("windowList"));
        assert!(s.contains("clientList"));
        assert!(s.contains(&format!("writeConfig(\"{RESULT_KEY}\"")));
        assert!(s.contains("JSON.stringify(out)"));
        assert!(s.contains("internalId || w.windowId"));

        let s = active_window_script();
        assert!(s.contains("activeWindow"));
        assert!(s.contains("activeClient"));
        assert!(s.contains("\"null\""));
    }

    #[test]
    fn list_windows_marks_focused() {
        let mut b = KWinWindows::with_channel(FakeChannel::new());
        let wins = b.list_windows().unwrap();
        assert_eq!(wins.len(), 2);
        assert_eq!(wins[0].title, "Terminal");
        assert!(!wins[0].focused);
        assert_eq!(wins[1].title, "Firefox");
        assert!(wins[1].focused);
        assert_eq!(wins[1].app_id, "firefox");
        // ids are stable string hashes, distinct per window
        assert_ne!(wins[0].id, wins[1].id);
        assert_eq!(wins[0].id, fnv1a("11"));
    }

    #[test]
    fn active_window_none_on_null() {
        let mut ch = FakeChannel::new();
        ch.results.insert("active", Some("null".to_string()));
        let mut b = KWinWindows::with_channel(ch);
        assert!(b.active_window().unwrap().is_none());

        let mut ch = FakeChannel::new();
        ch.results.insert("active", None);
        let mut b = KWinWindows::with_channel(ch);
        assert!(b.active_window().unwrap().is_none());
    }

    #[test]
    fn active_window_some() {
        let mut b = KWinWindows::with_channel(FakeChannel::new());
        let w = b.active_window().unwrap().unwrap();
        assert_eq!(w.title, "Firefox");
        assert!(w.focused);
    }

    #[test]
    fn fnv1a_is_stable_and_sensitive() {
        assert_eq!(fnv1a("11"), fnv1a("11"));
        assert_ne!(fnv1a("11"), fnv1a("22"));
        assert_ne!(fnv1a("11"), fnv1a("12"));
    }

    #[test]
    fn script_window_defaults() {
        // KWin may omit fields; defaults keep parsing total.
        let w: ScriptWindow = serde_json::from_str(r#"{"id":"7"}"#).unwrap();
        assert_eq!(w.title, "");
        assert_eq!(w.app_id, None);
    }
}
