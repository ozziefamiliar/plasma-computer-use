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
//!   over D-Bus with a temporary result callback.
//!
//! Everything here compiles on any Linux box; it only *runs* on a live
//! Plasma 6 session with the right tools and permissions (`/dev/uinput`
//! writable, `spectacle`, `kscreen-doctor`, a reachable KWin session bus).
//! Input setup probes eagerly; capture and window backends probe on use.
//!
//! KWin window queries have been live-tested on Plasma 6.7.5; capture and input
//! retain their separate `LIVE-VALIDATION` notes. The unit tests cover the parts
//! that don't need hardware: ioctl number math, the keycode table, PNG
//! header parsing, kscreen JSON parsing, and the KWin script generators.

pub mod kwin;
pub mod spectacle;
pub mod uinput;

pub use kwin::{DbusChannel, KWinWindows, QdbusChannel};
pub use spectacle::{KScreenDoctor, SpectacleCapture};
pub use uinput::UInputBackend;
