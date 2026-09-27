//! Optional `--debug` logging for the stdio server.
//!
//! One JSON object per `tools/call` batch, written to a caller-chosen sink
//! (stderr in the real host). Everything a post-mortem needs, nothing the
//! response stream needs:
//!
//! - `t`: unix timestamp (seconds, fractional) when the batch finished
//! - `event`: always `"batch"`
//! - `request_id`: the JSON-RPC request id, verbatim
//! - `backend`: the concrete backend/clock type names behind this server
//! - `requested`: the tool's `arguments` object, verbatim
//! - `elapsed_ms`: wall time spent inside the batch
//! - `result`: the MCP `CallToolResult` wire JSON — per-action status lines
//!   (`action`, `status`, `mapped` desktop coordinates, `error`), the
//!   self-describing screenshot payloads (frame id, width, height, scales),
//!   and `isError`
//!
//! Logging is best-effort: a sink write failure is swallowed so a full or
//! broken debug pipe can never fail a batch.

use serde_json::Value;
use std::io;

/// A JSON-lines sink. One `log` call writes exactly one line.
pub struct DebugLogger {
    out: Box<dyn io::Write>,
}

impl DebugLogger {
    pub fn new(out: impl io::Write + 'static) -> Self {
        Self { out: Box::new(out) }
    }

    /// Write one debug event as a single JSON line. Never fails the caller.
    pub fn log(&mut self, event: &Value) {
        let _ = writeln!(self.out, "{event}");
        let _ = self.out.flush();
    }
}

/// Unix timestamp, fractional seconds. std-only: no chrono dependency.
pub fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Build the `"batch"` debug event for one completed `tools/call`.
pub fn batch_event(
    request_id: &Value,
    arguments: &Value,
    result: &pcu_mcp::CallToolResult,
    backend: Value,
    elapsed_ms: f64,
) -> Value {
    serde_json::json!({
        "t": now_unix(),
        "event": "batch",
        "request_id": request_id,
        "backend": backend,
        "requested": arguments,
        "elapsed_ms": (elapsed_ms * 1000.0).round() / 1000.0,
        "result": result.to_json(),
    })
}

/// The concrete backend/clock types behind a `Server`, as JSON.
pub fn backend_names<C, I, W, K>() -> Value {
    serde_json::json!({
        "capture": std::any::type_name::<C>(),
        "input": std::any::type_name::<I>(),
        "window": std::any::type_name::<W>(),
        "clock": std::any::type_name::<K>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result() -> pcu_mcp::CallToolResult {
        pcu_mcp::CallToolResult {
            content: vec![pcu_mcp::Content::text("hello")],
            is_error: false,
        }
    }

    #[test]
    fn batch_event_carries_everything_g8_asked_for() {
        let id: Value = serde_json::json!(7);
        let args: Value = serde_json::json!({"actions": [{"type": "wait", "ms": 100}]});
        let backend = serde_json::json!({"capture": "c", "input": "i", "window": "w", "clock": "k"});
        let ev = batch_event(&id, &args, &sample_result(), backend.clone(), 12.3456789);
        assert_eq!(ev["event"], "batch");
        assert_eq!(ev["request_id"], 7);
        assert_eq!(ev["requested"], args);
        assert_eq!(ev["backend"], backend);
        assert!(ev["t"].as_f64().unwrap() > 1_700_000_000.0);
        assert_eq!(ev["elapsed_ms"], 12.346);
        assert_eq!(ev["result"]["isError"], false);
        assert_eq!(ev["result"]["content"][0]["text"], "hello");
    }

    #[test]
    fn logger_writes_one_line_per_log() {
        // Shared-buffer trick: Rc<RefCell<Vec<u8>>> readable after logging.
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Shared(Rc<RefCell<Vec<u8>>>);
        impl io::Write for Shared {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
        let mut logger = DebugLogger::new(Shared(shared.clone()));
        logger.log(&serde_json::json!({"a": 1}));
        logger.log(&serde_json::json!({"b": 2}));
        let text = String::from_utf8(shared.borrow().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(serde_json::from_str::<Value>(lines[0]).unwrap()["a"], 1);
    }
}
