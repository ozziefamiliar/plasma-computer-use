//! Window queries over KWin's session D-Bus scripting API.
//!
//! KWin provides `readConfig`, but no `writeConfig` global. Each query loads
//! a temporary script and returns its result through `callDBus` to a private
//! connection's unique bus name. No window titles enter kwinrc or the journal.
//! The callback is installed before running the script and accepts only the
//! current KWin owner's messages. Calls and result waits have bounded timeouts;
//! loaded scripts are unloaded on both success and failure.
//!
//! [`ScriptChannel`] keeps the transport replaceable. Window IDs remain FNV-1a
//! hashes of KWin's string IDs (with the existing hash-collision caveat).

use std::io::Write;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use dbus::blocking::Connection;
use dbus::channel::{MatchingReceiver, Sender};
use dbus::message::MatchRule;

use pcu_core::backend::{WindowBackend, WindowId, WindowInfo};
use pcu_core::result::ExecError;

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(3);
const CALLBACK_PATH: &str = "/org/pcu/ScriptResult";
const CALLBACK_INTERFACE: &str = "org.pcu.ScriptResult";

/// Evaluate a script using `pcuReport(json)` to return a payload.
///
/// `Ok(None)` may represent no active window in alternate implementations.
/// A missing callback or a script exception must be an error, not an empty
/// desktop. The production channel returns `Some("null")` for no active window.
pub trait ScriptChannel {
    fn eval(&mut self, script: &str) -> Result<Option<String>, ExecError>;
}

// ---- JS script generators (adapted from wdotool's recipe) ---------------------

fn list_windows_script() -> String {
    r#"
(function() {
  var out = [];
  var list = (typeof workspace.windowList === "function")
    ? workspace.windowList()
    : workspace.clientList();
  for (var i = 0; i < list.length; i++) {
    var w = list[i];
    var bounds = null;
    try {
      var fg = w.frameGeometry;
      if (fg) {
        bounds = { x: fg.x, y: fg.y, width: fg.width, height: fg.height };
      }
    } catch (e) {}
    out.push({
      id: (w.internalId || w.windowId || i).toString(),
      title: String(w.caption || ""),
      app_id: String(w.resourceClass || w.resourceName || ""),
      bounds: bounds
    });
  }
  pcuReport(JSON.stringify(out));
})();
"#
    .to_string()
}

fn active_window_script() -> String {
    r#"
(function() {
  var w = workspace.activeWindow || workspace.activeClient;
  var payload = "null";
  if (w) {
    payload = JSON.stringify({
      id: (w.internalId || w.windowId || 0).toString(),
      title: String(w.caption || ""),
      app_id: String(w.resourceClass || w.resourceName || "")
    });
  }
  pcuReport(payload);
})();
"#
    .to_string()
}

/// JSON-encode a string as a JS literal. `{:?}` would ~work for ASCII ids but
/// diverges from JS escape syntax on exotic codepoints; JSON strings are a
/// subset of JS string literals, so this is always safe to paste in.
fn js_string_literal(s: &str) -> String {
    serde_json::to_string(s).expect("serde_json cannot fail on &str")
}

fn activate_window_script(target_id: &str) -> String {
    format!(
        r#"
(function() {{
  var target = {target};
  var list = (typeof workspace.windowList === "function")
    ? workspace.windowList()
    : workspace.clientList();
  var found = false;
  for (var i = 0; i < list.length; i++) {{
    var w = list[i];
    var id = (w.internalId || w.windowId || i).toString();
    if (id === target) {{
      workspace.activeWindow = w;
      found = true;
      break;
    }}
  }}
  pcuReport(found ? "true" : "false");
}})();
"#,
        target = js_string_literal(target_id)
    )
}

#[derive(serde::Deserialize)]
struct ScriptBounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(serde::Deserialize)]
struct ScriptWindow {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    bounds: Option<ScriptBounds>,
}

/// FNV-1a 64: stable string→u64 for KWin's string window ids.
pub(crate) fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---- D-Bus callback channel -------------------------------------------------

/// Blocking session-bus transport; no async runtime or persistent service.
pub struct DbusChannel;

/// Compatibility name for clients of the original CLI-based transport.
pub use DbusChannel as QdbusChannel;

fn infra(stage: &str, error: impl std::fmt::Display) -> ExecError {
    ExecError::Infra(format!("KWin {stage}: {error}"))
}

fn callback_rule(owner: String) -> MatchRule<'static> {
    MatchRule::new_method_call()
        .with_strict_sender(owner)
        .with_path(CALLBACK_PATH)
        .with_interface(CALLBACK_INTERFACE)
        .with_member("Result")
}

fn callback_script(script: &str, destination: &str) -> String {
    // A D-Bus unique name is safe, but serialize it anyway rather than relying
    // on that when constructing JavaScript source.
    let destination = serde_json::to_string(destination).unwrap();
    format!(
        r#"(function() {{
  function report(ok, payload) {{
    callDBus({destination}, "{CALLBACK_PATH}", "{CALLBACK_INTERFACE}", "Result", ok, payload);
  }}
  function pcuReport(payload) {{ report(true, payload); }}
  try {{
{script}
  }} catch (error) {{
    report(false, String(error));
  }}
}})();
"#
    )
}

trait ScriptControl {
    fn load(&self, path: &str, plugin: &str) -> Result<i32, ExecError>;
    fn run(&self, id: i32) -> Result<(), ExecError>;
    fn unload(&self, plugin: &str) -> Result<(), ExecError>;
}

struct KWinControl<'a> {
    connection: &'a Connection,
    owner: String,
}

impl ScriptControl for KWinControl<'_> {
    fn load(&self, path: &str, plugin: &str) -> Result<i32, ExecError> {
        let (id,): (i32,) = self
            .connection
            .with_proxy(&*self.owner, "/Scripting", SCRIPT_TIMEOUT)
            .method_call("org.kde.kwin.Scripting", "loadScript", (path, plugin))
            .map_err(|e| infra("loadScript", e))?;
        Ok(id)
    }

    fn run(&self, id: i32) -> Result<(), ExecError> {
        self.connection
            .with_proxy(
                &*self.owner,
                format!("/Scripting/Script{id}"),
                SCRIPT_TIMEOUT,
            )
            .method_call("org.kde.kwin.Script", "run", ())
            .map_err(|e| infra("run", e))
    }

    fn unload(&self, plugin: &str) -> Result<(), ExecError> {
        let (_removed,): (bool,) = self
            .connection
            .with_proxy(&*self.owner, "/Scripting", SCRIPT_TIMEOUT)
            .method_call("org.kde.kwin.Scripting", "unloadScript", (plugin,))
            .map_err(|e| infra("unloadScript", e))?;
        Ok(())
    }
}

/// Always attempt unloading our unique plugin, even when load/run fails or
/// times out (the remote operation might already have taken effect).
fn run_script<T>(
    control: &impl ScriptControl,
    path: &str,
    plugin: &str,
    receive: impl FnOnce() -> Result<T, ExecError>,
) -> Result<T, ExecError> {
    let result = (|| {
        let id = control.load(path, plugin)?;
        if id < 0 {
            return Err(ExecError::Backend(format!("KWin loadScript returned {id}")));
        }
        control.run(id)?;
        receive()
    })();
    let cleanup = control.unload(plugin);
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(infra(
            "query and cleanup failed",
            format!("{error}; {cleanup}"),
        )),
    }
}

fn wait_result(
    receiver: &Receiver<Result<String, ExecError>>,
    timeout: Duration,
    mut process: impl FnMut(Duration) -> Result<(), ExecError>,
) -> Result<String, ExecError> {
    let start = Instant::now();
    loop {
        match receiver.try_recv() {
            Ok(result) => return result,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(infra("callback", "receiver disconnected"))
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(infra("callback", "timed out waiting for script result"));
        }
        process(remaining)?;
    }
}

impl ScriptChannel for DbusChannel {
    fn eval(&mut self, script: &str) -> Result<Option<String>, ExecError> {
        let connection = Connection::new_session().map_err(|e| infra("session bus", e))?;
        let (owner,): (String,) = connection
            .with_proxy(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                SCRIPT_TIMEOUT,
            )
            .method_call("org.freedesktop.DBus", "GetNameOwner", ("org.kde.KWin",))
            .map_err(|e| infra("service lookup", e))?;
        let (sender, receiver) = mpsc::channel();
        connection.start_receive(
            callback_rule(owner.clone()),
            Box::new(move |message, connection| {
                let result = match message.read2::<bool, String>() {
                    Ok((true, payload)) => Ok(payload),
                    Ok((false, error)) => Err(ExecError::Backend(format!(
                        "KWin script exception: {error}"
                    ))),
                    Err(error) => Err(infra("invalid callback", error)),
                };
                let reply = connection
                    .send(message.method_return())
                    .map_err(|_| infra("callback reply", "send failed"));
                let _ = sender.send(reply.and(result));
                false // one result per private connection
            }),
        );

        let mut file = tempfile::Builder::new()
            .prefix("pcu-kwin-")
            .suffix(".js")
            .tempfile()
            .map_err(|e| infra("temporary script", e))?;
        file.write_all(callback_script(script, &connection.unique_name()).as_bytes())
            .map_err(|e| infra("write script", e))?;
        let path = file
            .path()
            .to_str()
            .ok_or_else(|| infra("script path", "not UTF-8"))?;
        let plugin = file.path().file_name().unwrap().to_str().unwrap();
        let control = KWinControl {
            connection: &connection,
            owner,
        };
        // Keep the file alive until after unload: KWin reads scripts lazily.
        run_script(&control, path, plugin, || {
            wait_result(&receiver, SCRIPT_TIMEOUT, |remaining| {
                connection
                    .process(remaining)
                    .map(|_| ())
                    .map_err(|e| infra("callback dispatch", e))
            })
        })
        .map(Some)
    }
}

// ---- the backend ------------------------------------------------------------------

/// [`WindowBackend`] via KWin scripting. Generic over the [`ScriptChannel`]
/// so tests can drive it with canned scripts/results.
pub struct KWinWindows<C: ScriptChannel = DbusChannel> {
    channel: C,
}

impl KWinWindows<DbusChannel> {
    pub fn try_new() -> Self {
        Self {
            channel: DbusChannel,
        }
    }
}

impl<C: ScriptChannel> KWinWindows<C> {
    pub fn with_channel(channel: C) -> Self {
        Self { channel }
    }

    fn to_info(w: ScriptWindow, focused: bool) -> WindowInfo {
        WindowInfo {
            id: WindowId(fnv1a(&w.id)),
            title: w.title,
            app_id: w.app_id.unwrap_or_default(),
            focused,
            bounds: w.bounds.map(|b| pcu_core::backend::WindowBounds {
                x: b.x,
                y: b.y,
                w: b.width,
                h: b.height,
            }),
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
            .ok_or_else(|| infra("window list", "script returned no payload"))?;
        let windows = Self::parse_list(&payload)?;
        // Mark the focused one: compare against the active window's id.
        let active_id = match self.channel.eval(&active_window_script())? {
            Some(p) if p.trim() != "null" => Some(
                serde_json::from_str::<ScriptWindow>(&p)
                    .map_err(|e| ExecError::Backend(format!("invalid active-window payload: {e}")))?
                    .id,
            ),
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

    fn focus_window(&mut self, id: WindowId) -> Result<bool, ExecError> {
        // The trait's ids are FNV-1a hashes of KWin's string ids, which are
        // not invertible — so list first to recover the string id, then run
        // the targeted activate script (wdotool's `workspace.activeWindow =
        // w` recipe). The list's focused flags are discarded; focus itself
        // is asynchronous, so the caller confirms with `active_window`.
        let payload = self
            .channel
            .eval(&list_windows_script())?
            .unwrap_or_else(|| "[]".to_string());
        let target = Self::parse_list(&payload)?
            .into_iter()
            .find(|w| fnv1a(&w.id) == id.0);
        let Some(target) = target else {
            return Ok(false);
        };
        let payload = self.channel.eval(&activate_window_script(&target.id))?;
        Ok(payload
            .as_deref()
            .map(|p| p.trim() == "true")
            .unwrap_or(false))
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
                    r#"[{"id":"11","title":"Terminal","app_id":"org.kde.konsole","bounds":{"x":0,"y":0,"width":800,"height":600}},{"id":"22","title":"Firefox","app_id":"firefox","bounds":null}]"#.to_string(),
                ),
            );
            results.insert(
                "active",
                Some(r#"{"id":"22","title":"Firefox","app_id":"firefox"}"#.to_string()),
            );
            results.insert("activate", Some("true".to_string()));
            Self {
                results,
                seen_scripts: RefCell::new(Vec::new()),
            }
        }
    }

    impl ScriptChannel for FakeChannel {
        fn eval(&mut self, script: &str) -> Result<Option<String>, ExecError> {
            self.seen_scripts.borrow_mut().push(script.to_string());
            // the activate script contains "windowList" too — check it first
            let key = if script.contains("activeWindow = w") {
                "activate"
            } else if script.contains("windowList") {
                "list"
            } else {
                "active"
            };
            Ok(self.results[key].clone())
        }
    }

    #[test]
    fn scripts_use_windowlist_clientlist_dual_and_callback() {
        let s = list_windows_script();
        assert!(s.contains("windowList"));
        assert!(s.contains("clientList"));
        assert!(s.contains("pcuReport(JSON.stringify(out))"));
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
        assert_eq!(wins[0].id, WindowId(fnv1a("11")));
    }

    #[test]
    fn list_windows_reports_bounds() {
        let mut b = KWinWindows::with_channel(FakeChannel::new());
        let wins = b.list_windows().unwrap();
        let bounds = wins[0].bounds.unwrap();
        assert_eq!(bounds.w, 800.0);
        assert_eq!(bounds.h, 600.0);
        assert!(bounds.contains(10.0, 10.0));
        // null bounds stay None, not zero-sized
        assert!(wins[1].bounds.is_none());
    }

    #[test]
    fn activate_script_targets_id_and_reports() {
        let s = activate_window_script("22");
        assert!(s.contains("workspace.activeWindow = w"));
        assert!(s.contains("windowList"));
        assert!(s.contains("\"22\"")); // JS-safe string literal
        assert!(s.contains(r#"found ? "true" : "false""#));
    }

    #[test]
    fn activate_script_escapes_tricky_ids() {
        // a quote in the id must not break the JS string literal
        let s = activate_window_script("a\"; evil(); //");
        assert!(s.contains("a\\\"; evil(); //"));
    }

    #[test]
    fn focus_window_activates_known_id() {
        let ch = FakeChannel::new();
        let mut b = KWinWindows::with_channel(ch);
        assert!(b.focus_window(WindowId(fnv1a("22"))).unwrap());
        let seen = b.channel.seen_scripts.borrow();
        let activate: Vec<_> = seen
            .iter()
            .filter(|s| s.contains("activeWindow = w"))
            .collect();
        assert_eq!(activate.len(), 1);
        assert!(activate[0].contains("\"22\""));
    }

    #[test]
    fn focus_window_false_for_unknown_id() {
        let ch = FakeChannel::new();
        let mut b = KWinWindows::with_channel(ch);
        assert!(!b.focus_window(WindowId(fnv1a("nope"))).unwrap());
        // unknown id: no activate script ever ran
        assert!(!b
            .channel
            .seen_scripts
            .borrow()
            .iter()
            .any(|s| s.contains("activeWindow = w")));
    }

    #[test]
    fn focus_window_false_when_script_reports_miss() {
        let mut ch = FakeChannel::new();
        ch.results.insert("activate", Some("false".to_string()));
        let mut b = KWinWindows::with_channel(ch);
        // id exists in the list, but the script says it didn't find it
        assert!(!b.focus_window(WindowId(fnv1a("11"))).unwrap());
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

    #[test]
    fn missing_list_payload_is_not_an_empty_desktop() {
        let mut channel = FakeChannel::new();
        channel.results.insert("list", None);
        let error = KWinWindows::with_channel(channel)
            .list_windows()
            .unwrap_err();
        assert!(matches!(error, ExecError::Infra(_)));
    }

    #[test]
    fn malformed_focus_payload_is_not_silently_ignored() {
        let mut channel = FakeChannel::new();
        channel.results.insert("active", Some("broken".into()));
        assert!(KWinWindows::with_channel(channel).list_windows().is_err());
    }

    #[test]
    fn empty_desktop_and_no_focus_are_valid_explicit_results() {
        let mut channel = FakeChannel::new();
        channel.results.insert("list", Some("[]".into()));
        channel.results.insert("active", Some("null".into()));
        assert!(KWinWindows::with_channel(channel)
            .list_windows()
            .unwrap()
            .is_empty());
    }

    struct FakeControl {
        calls: RefCell<Vec<&'static str>>,
        fail: &'static str,
    }

    impl ScriptControl for FakeControl {
        fn load(&self, _path: &str, _plugin: &str) -> Result<i32, ExecError> {
            self.calls.borrow_mut().push("load");
            match self.fail {
                "load" => Err(infra("load", "failed")),
                "negative" => Ok(-1),
                _ => Ok(7),
            }
        }
        fn run(&self, id: i32) -> Result<(), ExecError> {
            assert_eq!(id, 7);
            self.calls.borrow_mut().push("run");
            if self.fail == "run" {
                Err(infra("run", "failed"))
            } else {
                Ok(())
            }
        }
        fn unload(&self, _plugin: &str) -> Result<(), ExecError> {
            self.calls.borrow_mut().push("unload");
            if self.fail == "unload" {
                Err(infra("unload", "failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn cleanup_runs_after_load_run_and_callback_failures() {
        for fail in ["none", "load", "negative", "run", "receive", "unload"] {
            let control = FakeControl {
                calls: RefCell::new(vec![]),
                fail,
            };
            let result = run_script(&control, "script.js", "unique-plugin", || {
                control.calls.borrow_mut().push("receive");
                if fail == "receive" {
                    Err(infra("callback", "timed out"))
                } else {
                    Ok("[]")
                }
            });
            assert_eq!(result.is_ok(), fail == "none", "{fail}");
            let expected = match fail {
                "load" | "negative" => vec!["load", "unload"],
                "run" => vec!["load", "run", "unload"],
                _ => vec!["load", "run", "receive", "unload"],
            };
            assert_eq!(*control.calls.borrow(), expected, "{fail}");
        }
    }

    #[test]
    fn callback_timeout_is_an_infrastructure_error() {
        let (_sender, receiver) = mpsc::channel();
        let result = wait_result(&receiver, Duration::ZERO, |_| {
            panic!("deadline already passed")
        });
        assert!(matches!(result, Err(ExecError::Infra(message)) if message.contains("timed out")));
    }

    #[test]
    fn callback_dispatch_preserves_payload_and_script_errors() {
        for payload in [
            Ok("héllo 🐺".to_string()),
            Err(ExecError::Backend("script error".into())),
        ] {
            let (sender, receiver) = mpsc::channel();
            let result = wait_result(&receiver, SCRIPT_TIMEOUT, |_| {
                sender.send(payload.clone()).unwrap();
                Ok(())
            });
            assert_eq!(result, payload);
        }
    }

    #[test]
    fn callback_dispatch_failure_is_reported() {
        let (_sender, receiver) = mpsc::channel();
        let error = infra("dispatch", "bus disconnected");
        assert_eq!(
            wait_result(&receiver, SCRIPT_TIMEOUT, |_| Err(error.clone())),
            Err(error)
        );
    }

    #[test]
    fn callback_only_accepts_the_pinned_kwin_owner_and_endpoint() {
        let rule = callback_rule(":1.42".into());
        for (sender, path, interface, member, accepted) in [
            (
                Some(":1.42"),
                CALLBACK_PATH,
                CALLBACK_INTERFACE,
                "Result",
                true,
            ),
            (
                Some(":1.43"),
                CALLBACK_PATH,
                CALLBACK_INTERFACE,
                "Result",
                false,
            ),
            (None, CALLBACK_PATH, CALLBACK_INTERFACE, "Result", false),
            (Some(":1.42"), "/wrong", CALLBACK_INTERFACE, "Result", false),
            (
                Some(":1.42"),
                CALLBACK_PATH,
                "org.pcu.Wrong",
                "Result",
                false,
            ),
            (
                Some(":1.42"),
                CALLBACK_PATH,
                CALLBACK_INTERFACE,
                "Wrong",
                false,
            ),
        ] {
            let mut message =
                dbus::Message::new_method_call(":1.99", path, interface, member).unwrap();
            message.set_sender(sender.map(Into::into));
            assert_eq!(rule.matches(&message), accepted);
        }
    }
}
