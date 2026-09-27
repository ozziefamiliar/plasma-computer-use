//! Read-only live KWin check; window titles are not printed.
use pcu_backends::KWinWindows;
use pcu_core::backend::WindowBackend;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let expected = match args.as_slice() {
        [] => None,
        [flag, title] if flag == "--expect-title" && !title.is_empty() => Some(title.as_str()),
        _ => return Err("usage: kwin_probe [--expect-title <title substring>]".into()),
    };
    let mut windows = KWinWindows::try_new();
    let started = Instant::now();
    let listed = loop {
        let listed = windows.list_windows()?;
        if expected.is_none_or(|title| listed.iter().any(|w| w.title.contains(title))) {
            break listed;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            return Err("expected window was not found".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let active = windows.active_window()?;
    println!(
        "{}",
        serde_json::json!({
            "window_count": listed.len(),
            "focused_count": listed.iter().filter(|w| w.focused).count(),
            "active_window_present": active.is_some(),
            "active_window_in_list": active.as_ref().map(|a| listed.iter().any(|w| w.id == a.id)),
            "expected_window_found": expected.map(|title| listed.iter().any(|w| w.title.contains(title))),
            "expected_window_focused": expected.map(|title| listed.iter().any(|w| w.title.contains(title) && w.focused)),
        })
    );
    Ok(())
}
