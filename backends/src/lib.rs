//! Real Plasma 6 backends for `pcu-core`.
//!
//! Design-against-mocks implementations of the three backend traits:
//!
//! - [`uinput::UInputBackend`] — absolute uinput pointer + wheel + keyboard
//!   (raw `/dev/uinput` ioctls, no daemon, no portal consent dialog).
//! - [`spectacle::SpectacleCapture`] — fullscreen capture via the
//!   `spectacle` CLI (the proven KDE route per the repo survey), with the
//!   coordinate space *measured* from `kscreen-doctor -j` output plus the
//!   PNG's own dimensions.
//! - [`kwin::KWinWindows`] — window listing via KWin scripting
//!   (`org.kde.kwin.Scripting.loadScript`, file-based on Plasma 6) driven
//!   over the `qdbus` CLI.
//!
//! Everything here compiles on any Linux box; it only *runs* on a live
//! Plasma 6 session with the right tools and permissions (`/dev/uinput`
//! writable, `spectacle`, `kscreen-doctor`, `qdbus`). Constructors are honest
//! about that: they probe and return [`pcu_core::result::ExecError::Infra`]
//! naming the missing piece instead of failing obscurely later.
//!
//! Nothing here is live-tested yet — that needs the Arch machine (see the
//! `LIVE-VALIDATION` notes in each module). The unit tests cover the parts
//! that don't need hardware: ioctl number math, the keycode table, PNG
//! header parsing, kscreen JSON parsing, and the KWin script generators.

pub mod kwin;
pub mod spectacle;
pub mod uinput;

pub use kwin::{KWinWindows, QdbusChannel};
pub use spectacle::{KScreenDoctor, SpectacleCapture};
pub use uinput::UInputBackend;
