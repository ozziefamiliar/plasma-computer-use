# KWin window-query validation

## Reproduced problem

On Arch Linux with KWin 6.7.5, a temporary KWin script reported:

```text
writeConfig=undefined readConfig=function callDBus=function windows=16
```

The original backend attempted `writeConfig("pcu_result", ...)`, polled
kwinrc, and eventually returned a successful empty list. The original
`kwin_probe` reported zero windows and no active window on that desktop.
This was a transport failure, not an empty desktop or a permission problem.

KDE's [KWin scripting API](https://develop.kde.org/docs/plasma/kwin/api/)
documents `readConfig` and asynchronous `callDBus`; it does not provide the
Plasma shell's `writeConfig` API in KWin scripts.

## Fix and scope

`DbusChannel` keeps the existing `ScriptChannel` seam and receives each query
through `callDBus` on a fresh session-bus connection. The callback accepts only
the pinned KWin service owner's messages at the expected path/interface/member.
Control calls target that same owner. Window titles are not written to kwinrc,
printed to the journal, or included in the diagnostic summary.

The receiver is installed before the temporary script runs. D-Bus method calls
and the callback wait each have a three-second timeout; this is not a single
three-second deadline for the entire query. Script exceptions and absent
callbacks are errors. Unloading is attempted even when loading, running, or
receiving fails, and the temporary file stays alive through cleanup.

`QdbusChannel` remains an alias for source compatibility. Implementations of
custom script channels should recognize `pcuReport(json)` in generated scripts
instead of `writeConfig`. The blocking `dbus` crate requires system libdbus
development files and pkg-config; `tempfile` supplies private temporary files.
There is no persistent helper service or async runtime.

Doctor now performs the actual window query rather than checking a CLI version.
Its check is named `kwin` rather than `qdbus`.

## Results on 2026-09-26

Environment: Arch Linux, KWin and Spectacle 6.7.5, KDE Wayland, one
2560×1440 output at scale 1.

| Check | Observed result |
|---|---|
| Original window backend | 0 windows, no active window (incorrect) |
| Fixed window backend | 16 windows, 1 focused, active ID present in list |
| Temporary KDialog with a unique `héllo 🐺` title | 17 windows; expected title found and focused |
| After terminating only the test dialog | 16 windows; active ID still present in list |
| Repeated Unicode callback | Exact payload received on three successive queries |
| Deliberate JavaScript exception | Explicit backend error |
| Deliberately omitted callback | Explicit infrastructure timeout after three seconds |
| Query after the timeout | Successful `[]` payload |
| Script inventory before/after live regression test | Unchanged |
| All five crate unit suites | 86 passed |
| Opt-in live regression test | 1 passed |
| Host doctor | All four checks passed, including actual KWin queries |

The fixture was closed automatically. No input events were sent. Doctor briefly
created and destroyed its virtual input devices. Device creation and the
Spectacle version check do **not** establish working input or screenshot capture.

## Reproduce in a live Plasma terminal

```bash
cargo run --locked --manifest-path backends/Cargo.toml --example kwin_probe
cargo test --locked --manifest-path backends/Cargo.toml --test kwin_live -- --ignored --nocapture
cargo run --locked --manifest-path host/Cargo.toml -- --doctor
```

The probe prints counts and ID-consistency booleans, not window titles. No active
window can be a legitimate desktop state. Listing and active-window queries are
separate snapshots, so changing focus or closing windows during a probe can
change their consistency fields. The live test compares the script inventory;
avoid loading/unloading unrelated KWin scripts while it runs.

For the known-window check, first build the example, then run this from the repo
root (requires Python 3 and `kdialog`). It closes only the dialog it creates:

```python
import json, subprocess, uuid

title = "PCU live check — héllo 🐺 " + uuid.uuid4().hex[:8]
dialog = subprocess.Popen([
    "kdialog", "--title", title, "--msgbox",
    "Temporary plasma-computer-use validation window. Closes automatically.",
])
try:
    result = subprocess.run([
        "backends/target/debug/examples/kwin_probe", "--expect-title", title,
    ], capture_output=True, text=True, check=True, timeout=15)
    report = json.loads(result.stdout)
    assert report["expected_window_found"]
    print(json.dumps(report))
finally:
    dialog.terminate()
    try:
        dialog.wait(timeout=5)
    except subprocess.TimeoutExpired:
        dialog.kill()
        dialog.wait()
```

Run against the user's live session bus. A sandbox can block D-Bus even when the
desktop works; a failed sandbox probe alone is not evidence of broken Plasma.

## Still unvalidated

Absolute pointer accuracy, click/drag/scroll delivery, keyboard layout behavior,
Unicode typing, screenshot pixel mapping, fractional/mixed-DPI layouts, and
other KWin versions remain separate work. Unicode in a window-query result
does not demonstrate Unicode keyboard input.
