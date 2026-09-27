# plasma-computer-use

A vision-first computer-use backend for **KDE Plasma 6 on Wayland**: screenshots
in, structured actions out, real input back into the desktop.

Built by [Ozzie](https://bevvy.lol) — an AI agent — as a background project.
The initial implementation took a few focused work sessions: 78 tests,
zero live machines harmed. The design
was informed by surveying existing Linux computer-use projects
(`agent-sh/computer-use-linux`, `Zetakai/desktop-mcp`,
`anaisbetts/mcp-computer-use`, `cushycush/wdotool`); the code here is original.

## Architecture

```
screenshots / frames
        │
        ▼
┌───────────────┐   JSON-RPC 2.0    ┌──────────────┐
│   pcu-stdio   │ ◄─── stdio ──────► │  agent client │
│  (pcu-host    │                    └──────────────┘
│   = real hw)  │
└───────┬───────┘
        │  Executor: frame-bound actions,
        │  stale coordinates hard-error,
        │  bounded infrastructure-only retries
        ▼
┌───────────────┐
│   pcu-core    │  frames · actions · coordinate transforms ·
│               │  backend traits · mock backends · batched executor
└───────┬───────┘
        ▼
┌───────────────┐
│ pcu-backends  │  UInputBackend (pointer/wheel/keyboard via uinput)
│               │  SpectacleCapture (spectacle + kscreen-doctor geometry)
│               │  KWinWindows (window queries over D-Bus)
└───────────────┘
        ▲
┌───────────────┐
│    pcu-mcp    │  thin MCP adapter over the same core
└───────────────┘
```

## Crates

| crate | what it is | tests |
|---|---|---|
| `core/` (`pcu-core`) | Frames, action types, coordinate transforms, results/errors, backend traits, mock backends, batched executor | 33 |
| `mcp/` (`pcu-mcp`) | Thin MCP adapter over the core | 8 |
| `stdio/` (`pcu-stdio`) | JSON-RPC 2.0 stdio server: `initialize`, `ping`, `tools/list`, `tools/call`; notification swallowing; proper JSON-RPC errors | 11 |
| `backends/` (`pcu-backends`) | Real Plasma 6 backends: uinput pointer/wheel/keyboard, Spectacle capture with measured geometry, KWin window queries | 30 |
| `host/` (`pcu-host`) | Binary wiring the real backends into the JSON-RPC stdio server, with a `--doctor` readiness report | 4 |

## Key design decisions

- **Frame-bound coordinates.** Every action is bound to a frame id; stale or
  out-of-frame coordinates hard-error and emit *no* input. The executor owns a
  `FrameRegistry` — nothing invents geometry.
- **Measured, never assumed.** The logical desktop bounding box comes from the
  same `kscreen-doctor` layout the capture backend measures with.
- **Timing policy:** ~250 ms movement settle, 90 ms press hold, 450 ms
  pre-screenshot quiescence, 24-step interpolated drags.
- **Bounded retries, infrastructure-only.** Retries never re-issue input that
  may have partially landed.
- **Keyboard the hard way.** evdev letter codes are keyboard-position ordered,
  not alphabetical — the mapping is a positional table with per-row regression
  tests, not arithmetic.
- **Readiness diagnostics.** `pcu-host --doctor` checks input-device creation,
  desktop geometry, Spectacle availability, and an actual KWin window-query
  round trip, then exits before serving anything.

## Build & test

Each crate is standalone (its own `Cargo.toml`). Building `backends` or `host`
also requires the libdbus development files and `pkg-config` (`dbus` and
`pkgconf` on Arch). The KWin transport uses blocking D-Bus calls; it needs no
async runtime, `qdbus`, or `kreadconfig6`.

```bash
cd core && cargo test        # 33 tests
cd ../mcp && cargo test      # 8 tests
cd ../stdio && cargo test    # 11 tests
cd ../backends && cargo test # 30 unit tests; live KWin test ignored by default
cd ../host && cargo test     # 4 tests
```

`pcu-host --doctor` prints the environment readiness report without starting
the server.

## Status

86 unit tests pass. **KWin window queries are live-validated on Arch Linux,
Plasma/KWin 6.7.5 Wayland**: window listing, active-window matching, a known
Unicode-titled window, callback errors/timeouts, and temporary-script cleanup.
See [the validation report and reproduction commands](docs/kwin-live-validation.md).

This does not validate pointer accuracy, keyboard/scroll behavior, screenshot
pixel mapping, or mixed-DPI layouts. Doctor's successful uinput check proves
device creation, not successful input delivery.

## Roadmap

- Doctor/diagnostics hardening, Unicode text handling (clipboard fallback —
  EIS owns the keymap on Wayland)
- Cleanup pass, live-test plan
- Continue live validation of capture and input on real hardware
