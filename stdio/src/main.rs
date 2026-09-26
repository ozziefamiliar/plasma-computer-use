//! `pcu-stdio`: demo binary wiring the pcu executor's mock backends to
//! the JSON-RPC stdio loop from `pcu_stdio`.
//!
//! A real-backend host will construct `pcu_stdio::Server` with its own
//! `CaptureBackend`/`InputBackend`/`WindowBackend`/`Clock` and call
//! `serve()`; the framing code is shared, only the backends differ.
//!
//! The mock capture returns synthetic bytes (not a real image), so the
//! default mime type is `application/octet-stream`; override with
//! `--mime <type>`, e.g. when a real backend supplies PNG.

use pcu_core::{
    CoordSpace, DesktopGeometry, Executor, MockCapture, MockClock, MockInput, MockWindow, Timing,
};
use pcu_stdio::{serve, Server};
use std::io::{self, BufReader};

fn main() -> io::Result<()> {
    let mut mime = "application/octet-stream".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mime" => {
                mime = args.next().unwrap_or_else(|| {
                    eprintln!("--mime requires a value");
                    std::process::exit(2);
                });
            }
            "--help" | "-h" => {
                println!("pcu-stdio: JSON-RPC 2.0 stdio server for the pcu MCP adapter.\n\nUsage: pcu-stdio [--mime <type>]");
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    // Mock geometry: 1920x1080 at 1.0 scale. Real backends will report
    // the measured capture space instead.
    let space = CoordSpace {
        image_w: 1920,
        image_h: 1080,
        origin_x: 0.0,
        origin_y: 0.0,
        scale_x: 1.0,
        scale_y: 1.0,
    };
    let exec = Executor::new(
        MockCapture::new(space),
        MockInput::new(),
        MockWindow::default(),
        MockClock::new(),
        Timing::default(),
        DesktopGeometry::single(1920.0, 1080.0),
    );
    let mut server = Server::new(exec, mime);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    serve(&mut server, BufReader::new(stdin.lock()), &mut stdout)
}
