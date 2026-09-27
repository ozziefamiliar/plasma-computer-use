//! Opt-in live checks: no input events, configuration writes, or title logging.
use dbus::blocking::Connection;
use pcu_backends::kwin::{DbusChannel, ScriptChannel};
use pcu_core::result::ExecError;
use std::time::{Duration, Instant};

fn script_inventory(connection: &Connection) -> String {
    let (xml,): (String,) = connection
        .with_proxy("org.kde.KWin", "/Scripting", Duration::from_secs(3))
        .method_call("org.freedesktop.DBus.Introspectable", "Introspect", ())
        .expect("KWin scripting interface must be reachable");
    xml
}

#[test]
#[ignore = "requires a live Plasma session; loads temporary read-only KWin scripts"]
fn callback_success_exception_timeout_and_cleanup() {
    let connection = Connection::new_session().unwrap();
    let before = script_inventory(&connection);
    let mut channel = DbusChannel;
    for _ in 0..3 {
        assert_eq!(
            channel.eval("pcuReport('héllo 🐺');").unwrap(),
            Some("héllo 🐺".into())
        );
    }
    let error = channel
        .eval("throw new Error('pcu deliberate test');")
        .unwrap_err();
    assert!(
        matches!(error, ExecError::Backend(message) if message.contains("pcu deliberate test"))
    );

    let start = Instant::now();
    let error = channel
        .eval("/* deliberately omit the callback */")
        .unwrap_err();
    assert!(matches!(error, ExecError::Infra(message) if message.contains("timed out")));
    assert!(start.elapsed() >= Duration::from_secs(3));
    assert_eq!(channel.eval("pcuReport('[]');").unwrap(), Some("[]".into()));

    // KWin can defer QObject deletion to its next event-loop iteration.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let after = script_inventory(&connection);
        if after == before {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "KWin script inventory did not return to its initial state"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
