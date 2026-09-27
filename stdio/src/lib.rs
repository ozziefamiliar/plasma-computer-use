//! Thin line-delimited JSON-RPC 2.0 stdio transport for the pcu MCP adapter.
//!
//! One request per line on stdin, one response (or none, for notifications)
//! per line on stdout. This is framing only: MCP method routing is
//! [`handle_request`], and the actual tool semantics live in `pcu_mcp`. A
//! host that speaks HTTP/SSE or any other transport can reuse the same
//! router with its own line splitting.
//!
//! Handled methods: `initialize`, `notifications/initialized` (no reply),
//! `tools/list`, `tools/call`, `notifications/cancelled` (arms emergency
//! cancellation, no reply), `ping`. Anything else is JSON-RPC
//! `-32601 Method not found`. Transport-level tool problems (unknown tool,
//! bad arguments) do **not** become JSON-RPC errors: `pcu_mcp::call_tool`
//! reports them as MCP `isError` results, keeping the wire level for wire
//! problems only.

use pcu_core::{
    CaptureBackend, Clock, Executor, InputBackend, WindowBackend,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Optional `--debug` JSON-lines logging for `tools/call` batches.
pub mod debug;

/// The MCP protocol version this loop claims to speak.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A request router plus its executor. Generic over backends so the
/// mock-wired demo binary and a future real-backend host share the exact
/// same framing code.
pub struct Server<C, I, W, K>
where
    C: CaptureBackend,
    I: InputBackend,
    W: WindowBackend,
    K: Clock,
{
    pub exec: Executor<C, I, W, K>,
    /// Mime type named on screenshot image content. Caller-supplied because
    /// capture bytes are opaque to the core (e.g. `"image/png"` for a real
    /// PNG backend, `"application/octet-stream"` for the mock demo).
    pub screenshot_mime: String,
    /// Optional debug sink. `None` (the default) disables logging;
    /// [`Server::with_debug`] attaches one.
    debug: Option<debug::DebugLogger>,
}

impl<C, I, W, K> Server<C, I, W, K>
where
    C: CaptureBackend,
    I: InputBackend,
    W: WindowBackend,
    K: Clock,
{
    pub fn new(exec: Executor<C, I, W, K>, screenshot_mime: impl Into<String>) -> Self {
        Self {
            exec,
            screenshot_mime: screenshot_mime.into(),
            debug: None,
        }
    }

    /// Attach a debug sink: every `tools/call` batch then emits one JSON
    /// line (request id, backends, requested actions, elapsed ms, result
    /// with frame ids/dims/mapped coords/failures) to `sink`.
    pub fn with_debug(mut self, sink: impl std::io::Write + 'static) -> Self {
        self.debug = Some(debug::DebugLogger::new(sink));
        self
    }
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

fn ok(id: &Value, result: Value) -> Option<String> {
    Some(
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
            .to_string(),
    )
}

fn err(id: &Option<Value>, code: i64, message: &str) -> Option<String> {
    let id_json = id.clone().unwrap_or(Value::Null);
    Some(
        json!({
            "jsonrpc": "2.0",
            "id": id_json,
            "error": { "code": code, "message": message },
        })
        .to_string(),
    )
}

/// Handle one JSON-RPC 2.0 request line.
///
/// Returns `None` when the line is a notification (no response per spec) or
/// is otherwise unanswerable. Returns `Some` response line otherwise.
pub fn handle_request<C, I, W, K>(server: &mut Server<C, I, W, K>, line: &str) -> Option<String>
where
    C: CaptureBackend,
    I: InputBackend,
    W: WindowBackend,
    K: Clock,
{
    let req: RpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        // -32700 Parse error. For a request (has id) we can still answer
        // only if the id was legible — it wasn't, so null it per spec.
        Err(_) => {
            // Try to salvage an id for a non-notification; cheap attempt,
            // otherwise null.
            let id = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| v.get("id").cloned());
            return err(&id, -32700, "parse error: not valid JSON");
        }
    };

    let is_notification = req.id.is_none();
    let id = req.id.unwrap_or(Value::Null);
    let params = req.params.unwrap_or(Value::Null);

    // Route, producing (code, message, result) — notifications swallow all.
    let outcome: Result<Value, (i64, String)> = match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "pcu-stdio", "version": env!("CARGO_PKG_VERSION") },
        })),
        "notifications/initialized" => {
            return None; // notification: no response, ever
        }
        // MCP cancellation: params carry {"requestId": <id>}. Arming the
        // executor's cancel flag drains the batch at the next action
        // boundary; the flag is one-shot, so a cancel for an already
        // finished request can't poison a later one. Note the transport
        // limit: this single-threaded loop can't read the cancel line
        // until the in-flight batch returns, so the flag takes effect at
        // the first boundary of the *next* batch. A host that runs
        // execute() on a worker thread and hands it a CancelHandle gets
        // true mid-batch cancellation.
        "notifications/cancelled" => {
            if let Some(dbg) = server.debug.as_mut() {
                dbg.log(
                    &serde_json::json!({"event": "cancel", "request_id": params.get("requestId")}),
                );
            }
            server.exec.cancel();
            return None; // notification: no response, ever
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(pcu_mcp::tools_list()),
        "tools/call" => {
            let name = match params.get("name").and_then(Value::as_str) {
                Some(n) => n,
                None => {
                    return if is_notification {
                        None
                    } else {
                        err(&Some(id), -32602, "tools/call requires params.name (string)")
                    }
                }
            };
            let arguments = params.get("arguments").unwrap_or(&Value::Null);
            let t0 = std::time::Instant::now();
            let result = pcu_mcp::call_tool(
                &mut server.exec,
                name,
                arguments,
                &server.screenshot_mime,
            );
            if let Some(dbg) = server.debug.as_mut() {
                dbg.log(&debug::batch_event(
                    &id,
                    arguments,
                    &result,
                    debug::backend_names::<C, I, W, K>(),
                    t0.elapsed().as_secs_f64() * 1000.0,
                ));
            }
            Ok(result.to_json())
        }
        other => Err((-32601, format!("method not found: {:?}", other))),
    };

    if is_notification {
        // JSON-RPC: notifications never get a response, even on error.
        return None;
    }
    match outcome {
        Ok(result) => ok(&id, result),
        Err((code, message)) => err(&Some(id), code, &message),
    }
}

/// Serve the stdio loop until EOF: read request lines from `input`, write
/// response lines to `output`. Generic over I/O so tests can use memory
/// buffers; the binary wires `stdin`/`stdout`.
pub fn serve<C, I, W, K, R, Out>(
    server: &mut Server<C, I, W, K>,
    input: R,
    output: &mut Out,
) -> std::io::Result<()>
where
    C: CaptureBackend,
    I: InputBackend,
    W: WindowBackend,
    K: Clock,
    R: std::io::BufRead,
    Out: std::io::Write,
{
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_request(server, &line) {
            writeln!(output, "{response}")?;
        }
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcu_core::{
        CoordSpace, DesktopGeometry, MockCapture, MockClock, MockInput, MockWindow, Timing,
    };

    type MockServer = Server<MockCapture, MockInput, MockWindow, MockClock>;

    fn mock_server() -> MockServer {
        let space = CoordSpace {
            image_w: 1920,
            image_h: 1080,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        };
        Server::new(
            Executor::new(
                MockCapture::new(space),
                MockInput::new(),
                MockWindow::default(),
                MockClock::new(),
                Timing::default(),
                DesktopGeometry::single(1920.0, 1080.0),
            ),
            "application/octet-stream",
        )
    }

    fn resp(line: &str) -> Value {
        serde_json::from_str(line).expect("response line is JSON")
    }

    #[test]
    fn initialize_handshake() {
        let mut s = mock_server();
        let line = handle_request(
            &mut s,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        )
        .expect("initialize gets a response");
        let v = resp(&line);
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "pcu-stdio");
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialized_notification_has_no_response() {
        let mut s = mock_server();
        assert_eq!(
            handle_request(
                &mut s,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            ),
            None
        );
    }

    #[test]
    fn tools_list_routes_to_adapter() {
        let mut s = mock_server();
        let line = handle_request(
            &mut s,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        )
        .expect("response");
        let v = resp(&line);
        assert_eq!(v["result"]["tools"][0]["name"], "computer_use");
    }

    #[test]
    fn tools_call_screenshot_returns_mcp_result() {
        let mut s = mock_server();
        let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name": "computer_use",
            "arguments": {"actions": [
                {"type": "screenshot", "note": null},
                {"type": "click", "frame": 1, "button": "left", "x": 960, "y": 540}
            ]}
        }}"#;
        let line = handle_request(&mut s, req).expect("response");
        let v = resp(&line);
        assert_eq!(v["id"], 3);
        // Transport-level success: isError false, 4 content items
        // (screenshot status, desc, image, click status).
        assert_eq!(v["result"]["isError"], false);
        assert_eq!(v["result"]["content"].as_array().unwrap().len(), 4);
        assert_eq!(
            v["result"]["content"][2]["mimeType"],
            "application/octet-stream"
        );
    }

    #[test]
    fn unknown_tool_is_mcp_error_not_jsonrpc_error() {
        let mut s = mock_server();
        let line = handle_request(
            &mut s,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        )
        .expect("response");
        let v = resp(&line);
        // Still a JSON-RPC *result*; the error lives in MCP's isError.
        assert!(v.get("error").is_none());
        assert_eq!(v["result"]["isError"], true);
    }

    #[test]
    fn unknown_method_is_jsonrpc_error() {
        let mut s = mock_server();
        let line = handle_request(
            &mut s,
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#,
        )
        .expect("response");
        let v = resp(&line);
        assert_eq!(v["error"]["code"], -32601);
        assert!(v["error"]["message"].as_str().unwrap().contains("resources/list"));
    }

    #[test]
    fn bad_params_is_invalid_params() {
        let mut s = mock_server();
        let line = handle_request(
            &mut s,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":42}}"#,
        )
        .expect("response");
        let v = resp(&line);
        assert_eq!(v["error"]["code"], -32602);
    }

    #[test]
    fn parse_error_uses_null_id() {
        let mut s = mock_server();
        let line = handle_request(&mut s, "this is not json").expect("response");
        let v = resp(&line);
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["id"], Value::Null);
    }

    #[test]
    fn notification_never_gets_a_response_even_on_error() {
        let mut s = mock_server();
        // no id => notification; unknown method must still be swallowed
        assert_eq!(
            handle_request(&mut s, r#"{"jsonrpc":"2.0","method":"nope"}"#),
            None
        );
        // malformed JSON can't even be identified; parse error answers with
        // null id (it can't be proven to be a notification)
        let line = handle_request(&mut s, "{bad").expect("response");
        assert_eq!(resp(&line)["error"]["code"], -32700);
    }

    #[test]
    fn serve_end_to_end_over_buffers() {
        let mut s = mock_server();
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"computer_use\",\"arguments\":{\"actions\":[{\"type\":\"screenshot\",\"note\":null}]}}}\n",
        );
        let mut out = Vec::new();
        serve(&mut s, input.as_bytes(), &mut out).expect("serve runs");
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // notification + blank line produce no output: 3 responses
        assert_eq!(lines.len(), 3);
        let ids: Vec<i64> = lines
            .iter()
            .map(|l| resp(l)["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
        // tools/call screenshot: status + desc + image = 3 content items
        assert_eq!(resp(lines[2])["result"]["content"].as_array().unwrap().len(), 3);
    }

    /// `notifications/cancelled` arms emergency cancellation (no response),
    /// and the next batch drains every action as cancelled; the flag is
    /// one-shot, so the batch after that runs clean.
    #[test]
    fn cancelled_notification_arms_executor() {
        let mut s = mock_server();
        // Notification: no id, no response — even though it names a request.
        assert_eq!(
            handle_request(
                &mut s,
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#
            ),
            None
        );
        let req = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{
            "name": "computer_use",
            "arguments": {"actions": [
                {"type": "screenshot", "note": null},
                {"type": "click", "frame": 1, "button": "left", "x": 10, "y": 10}
            ]}
        }}"#;
        let line = handle_request(&mut s, req).expect("response");
        let v = resp(&line);
        assert_eq!(v["id"], 8);
        let content = v["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        for item in content {
            let status: Value =
                serde_json::from_str(item["text"].as_str().unwrap()).unwrap();
            assert_eq!(status["status"], "error");
            assert!(status["error"].as_str().unwrap().contains("cancelled"));
        }
        // One-shot: the following batch runs normally.
        let line = handle_request(&mut s, req).expect("response");
        let v = resp(&line);
        assert_eq!(v["result"]["isError"], false);
        let content = v["result"]["content"].as_array().unwrap();
        let status: Value =
            serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(status["status"], "ok");
    }

    #[test]
    fn ping_round_trips() {
        let mut s = mock_server();
        let line = handle_request(&mut s, r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#)
            .expect("response");
        let v = resp(&line);
        assert_eq!(v["id"], 9);
        assert_eq!(v["result"], json!({}));
    }

    /// `with_debug` server: one JSON debug line per tools/call batch, with
    /// frame id, dims, mapped coords, requested actions, backends, timing,
    /// and failures (here: none).
    #[test]
    fn debug_logs_one_line_per_batch() {
        use std::cell::RefCell;
        use std::io::Write;
        use std::rc::Rc;
        struct Shared(Rc<RefCell<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
        let mut s = mock_server().with_debug(Shared(shared.clone()));
        let req = r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{
            "name": "computer_use",
            "arguments": {"actions": [
                {"type": "screenshot", "note": null},
                {"type": "click", "frame": 1, "button": "left", "x": 960, "y": 540}
            ]}
        }}"#;
        handle_request(&mut s, req).expect("response");

        let text = String::from_utf8(shared.borrow().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "exactly one debug line per batch");
        let ev: Value = serde_json::from_str(lines[0]).expect("debug line is JSON");

        assert_eq!(ev["event"], "batch");
        assert_eq!(ev["request_id"], 42);
        assert!(ev["elapsed_ms"].as_f64().unwrap() >= 0.0);
        // Backend type names name the mock backends.
        assert!(ev["backend"]["capture"].as_str().unwrap().contains("MockCapture"));
        assert!(ev["backend"]["input"].as_str().unwrap().contains("MockInput"));
        // Requested actions verbatim.
        assert_eq!(ev["requested"]["actions"].as_array().unwrap().len(), 2);
        assert_eq!(ev["requested"]["actions"][0]["type"], "screenshot");
        // Result: screenshot status + self-describing payload + image +
        // click status with mapped desktop coords.
        assert_eq!(ev["result"]["isError"], false);
        let content = ev["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 4);
        let desc: Value =
            serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(desc["frame_id"], 1);
        assert_eq!(desc["width"], 1920);
        assert_eq!(desc["height"], 1080);
        let click: Value =
            serde_json::from_str(content[3]["text"].as_str().unwrap()).unwrap();
        assert_eq!(click["status"], "ok");
        assert_eq!(click["mapped"][0]["x"], 960.0);
        assert_eq!(click["mapped"][0]["y"], 540.0);
    }

    /// Debug lines also record failures: here a click on an unknown frame.
    #[test]
    fn debug_line_records_failures() {
        use std::cell::RefCell;
        use std::io::Write;
        use std::rc::Rc;
        struct Shared(Rc<RefCell<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
        let mut s = mock_server().with_debug(Shared(shared.clone()));
        let req = r#"{"jsonrpc":"2.0","id":43,"method":"tools/call","params":{
            "name": "computer_use",
            "arguments": {"actions": [
                {"type": "click", "frame": 99, "button": "left", "x": 10, "y": 10}
            ]}
        }}"#;
        handle_request(&mut s, req).expect("response");

        let text = String::from_utf8(shared.borrow().clone()).unwrap();
        let ev: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(ev["result"]["isError"], false); // per-action, not transport
        let content = ev["result"]["content"].as_array().unwrap();
        let status: Value =
            serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(status["status"], "error");
        assert!(status["error"].as_str().unwrap().contains("99"));
    }
}
