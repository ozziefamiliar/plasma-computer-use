//! Thin MCP transport adapter for the pcu executor.
//!
//! This is message-shaping only: it maps an MCP `tools/call` request onto a
//! [`Batch`](pcu_core::Batch) and a
//! [`BatchResult`](pcu_core::BatchResult) back onto MCP `content` items. It
//! does not run an MCP server loop, own stdio, or know anything about
//! JSON-RPC framing — the host wires [`tools_list`] and [`call_tool`] into
//! whatever transport it uses.
//!
//! Design notes (from the anaisbetts-mcp-computer-use survey, chew session
//! #1):
//!
//! - **One batched `computer_use` tool, not one tool per action.** The
//!   model's batched `actions[]` shape goes straight to the executor; each
//!   action's outcome is reported as its own status line in the result.
//! - **Screenshots are self-describing.** A successful `screenshot` action
//!   yields an image `content` item *plus* the `ScreenshotDesc` payload
//!   (frame id, dims, scale) so later coordinate actions can bind to the
//!   frame id. Image bytes are base64-encoded per the MCP spec; the mime
//!   type is the caller's to name, since capture bytes are opaque to the
//!   core.
//! - **Partial success, not whole-call failure.** A failed action never
//!   aborts its batch (the executor's contract), and it does not fail the
//!   tool call either: `is_error` stays false and the per-action status
//!   line carries the detail. `is_error` is reserved for transport-level
//!   problems: unknown tool, unparseable arguments.

use pcu_core::{
    Action, ActionOutcome, Batch, CaptureBackend, Clock, DesktopPoint, Executor, FrameId,
    InputBackend, WindowBackend,
};
use serde::Serialize;

/// The single tool this adapter exposes.
pub const TOOL_NAME: &str = "computer_use";

/// The arguments a `computer_use` call carries: a batch of actions.
#[derive(Debug, serde::Deserialize)]
struct ComputerUseArgs {
    actions: Vec<Action>,
}

/// One MCP `content` item: text or image, in the wire shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text { text: String },
    Image { data: String, #[serde(rename = "mimeType")] mime_type: String },
}

impl Content {
    pub fn text(t: impl Into<String>) -> Self {
        Content::Text { text: t.into() }
    }

    pub fn image(base64_data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Content::Image {
            data: base64_data.into(),
            mime_type: mime_type.into(),
        }
    }
}

/// The result of one `tools/call`, in MCP's `CallToolResult` shape.
#[derive(Debug, Clone, PartialEq)]
pub struct CallToolResult {
    pub content: Vec<Content>,
    pub is_error: bool,
}

impl CallToolResult {
    /// The wire JSON: `{"content": [...], "isError": bool}`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "content": self.content,
            "isError": self.is_error,
        })
    }

    fn tool_error(message: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(message.into())],
            is_error: true,
        }
    }
}

/// Per-action status line, serialized as a JSON text item.
#[derive(Debug, Serialize)]
struct ActionStatus<'a> {
    action: &'a str,
    status: &'static str, // "ok" | "noop" | "error"
    #[serde(skip_serializing_if = "Option::is_none")]
    mapped: Option<&'a Vec<DesktopPoint>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn action_name(a: &Action) -> &'static str {
    match a {
        Action::Screenshot { .. } => "screenshot",
        Action::Move { .. } => "move",
        Action::Click { .. } => "click",
        Action::DoubleClick { .. } => "double_click",
        Action::Drag { .. } => "drag",
        Action::Scroll { .. } => "scroll",
        Action::Keypress { .. } => "keypress",
        Action::Type { .. } => "type",
        Action::Wait { .. } => "wait",
    }
}

/// Route one MCP tool call through the executor.
///
/// `arguments` is the tool's `arguments` object (`{"actions": [...]}`).
/// `screenshot_mime_type` names the mime type of the capture backend's
/// bytes (e.g. `"image/png"`); the core treats image bytes as opaque, so
/// the caller supplies it.
pub fn call_tool<C, I, W, K>(
    exec: &mut Executor<C, I, W, K>,
    name: &str,
    arguments: &serde_json::Value,
    screenshot_mime_type: &str,
) -> CallToolResult
where
    C: CaptureBackend,
    I: InputBackend,
    W: WindowBackend,
    K: Clock,
{
    if name != TOOL_NAME {
        return CallToolResult::tool_error(format!(
            "unknown tool {:?}; this adapter exposes only {:?}",
            name, TOOL_NAME
        ));
    }

    let args: ComputerUseArgs = match serde_json::from_value(arguments.clone()) {
        Ok(a) => a,
        Err(e) => {
            return CallToolResult::tool_error(format!(
                "invalid arguments for {:?}: {}",
                TOOL_NAME, e
            ))
        }
    };

    let result = exec.execute(&Batch(args.actions.clone()));
    let mut content = Vec::new();
    let mut new_frames = result.new_frames.iter();

    for (action, outcome) in args.actions.iter().zip(result.outcomes.iter()) {
        let (status, mapped, error) = match outcome {
            ActionOutcome::Done { mapped } => ("ok", Some(mapped), None),
            ActionOutcome::NoOp => ("noop", None, None),
            ActionOutcome::Failed { error } => ("error", None, Some(error.to_string())),
        };
        content.push(Content::text(
            serde_json::to_string(&ActionStatus {
                action: action_name(action),
                status,
                mapped,
                error,
            })
            .expect("ActionStatus serializes"),
        ));

        // A successful screenshot appends its self-describing payload and
        // the image bytes, so the model gets pixels + frame id together.
        if matches!(action, Action::Screenshot { .. })
            && matches!(outcome, ActionOutcome::Done { .. })
        {
            if let Some(id) = new_frames.next().copied() {
                append_screenshot(exec, id, &mut content, screenshot_mime_type);
            }
        }
    }

    CallToolResult {
        content,
        is_error: false,
    }
}

/// Append the `ScreenshotDesc` text item and the base64 image item for a
/// newly captured frame.
fn append_screenshot<C, I, W, K>(
    exec: &Executor<C, I, W, K>,
    id: FrameId,
    content: &mut Vec<Content>,
    mime_type: &str,
) where
    C: CaptureBackend,
    I: InputBackend,
    W: WindowBackend,
    K: Clock,
{
    if let Some(desc) = exec.describe_frame(id) {
        content.push(Content::text(
            serde_json::to_string(&desc).expect("ScreenshotDesc serializes"),
        ));
    }
    if let Some(bytes) = exec.screenshot_bytes(id) {
        content.push(Content::image(base64::encode(bytes), mime_type));
    }
}

/// The `tools/list` response: the one tool this adapter exposes.
pub fn tools_list() -> serde_json::Value {
    serde_json::json!({
        "tools": [{
            "name": TOOL_NAME,
            "description": "Drive the local Plasma desktop: run a batch of computer-use actions (screenshot, move, click, drag, scroll, keypress, type, wait). Coordinate actions take screenshot-space pixels plus the frame id from a prior screenshot's self-describing payload; a failed action never aborts the rest of the batch.",
            "inputSchema": input_schema(),
        }]
    })
}

/// JSON Schema for `{"actions": [...]}`. Hand-written: the shapes mirror the
/// `Action` enum's serde representation exactly.
fn input_schema() -> serde_json::Value {
    /// Schema for a coordinate action: the type tag plus frame + x/y, with
    /// any action-specific extra properties merged in.
    fn coord_schema(type_name: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut props = serde_json::json!({
            "type": {"const": type_name},
            "frame": {"type": "integer", "description": "Frame id from a screenshot's self-describing payload. Coordinates are pixels in that screenshot's image space."},
            "x": {"type": "integer"},
            "y": {"type": "integer"},
        });
        if let (Some(map), serde_json::Value::Object(extra_map)) =
            (props.as_object_mut(), extra)
        {
            map.extend(extra_map);
        }
        props
    }
    let button = serde_json::json!({
        "button": {"type": "string", "enum": ["left", "right", "middle"]}
    });
    serde_json::json!({
        "type": "object",
        "properties": {
            "actions": {
                "type": "array",
                "description": "Actions to run in order. A screenshot action mid-batch registers a frame that later actions in the same batch may reference.",
                "items": {"oneOf": [
                    {"type": "object", "properties": {
                        "type": {"const": "screenshot"},
                        "note": {"type": ["string", "null"], "description": "Free-form context for the audit/debug log."}
                    }, "required": ["type"]},
                    {"type": "object",
                     "properties": coord_schema("move", serde_json::json!({})),
                     "required": ["type", "frame", "x", "y"]},
                    {"type": "object",
                     "properties": coord_schema("click", button.clone()),
                     "required": ["type", "frame", "button", "x", "y"]},
                    {"type": "object",
                     "properties": coord_schema("double_click", button.clone()),
                     "required": ["type", "frame", "button", "x", "y"]},
                    {"type": "object", "properties": {
                        "type": {"const": "drag"},
                        "frame": {"type": "integer"},
                        "path": {"type": "array", "items": {
                            "type": "object",
                            "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}},
                            "required": ["x", "y"]
                        }}
                    }, "required": ["type", "frame", "path"]},
                    {"type": "object",
                     "properties": coord_schema("scroll", serde_json::json!({
                        "dx": {"type": "number", "description": "Horizontal scroll ticks (positive = right)."},
                        "dy": {"type": "number", "description": "Vertical scroll ticks (positive = down)."}
                    })),
                     "required": ["type", "frame", "x", "y", "dx", "dy"]},
                    {"type": "object", "properties": {
                        "type": {"const": "keypress"},
                        "keys": {"type": "array", "items": {"type": "string"},
                                 "description": "Key combination, e.g. [\"CTRL\", \"L\"]. Unresolvable names are a backend error, never a silent no-op."}
                    }, "required": ["type", "keys"]},
                    {"type": "object", "properties": {
                        "type": {"const": "type"},
                        "text": {"type": "string", "description": "Literal Unicode text to insert."}
                    }, "required": ["type", "text"]},
                    {"type": "object", "properties": {
                        "type": {"const": "wait"},
                        "ms": {"type": "integer", "description": "Sleep before the next action in the batch."}
                    }, "required": ["type", "ms"]}
                ]}
            }
        },
        "required": ["actions"]
    })
}

/// Minimal standard base64 encoder (RFC 4648 §4), kept dependency-free so
/// the adapter builds offline. Only encoding is needed: the model never
/// sends us base64.
mod base64 {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
        for chunk in input.chunks(3) {
            let mut n: u32 = 0;
            for &b in chunk {
                n = (n << 8) | b as u32;
            }
            n <<= 8 * (3 - chunk.len());
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rfc4648_vectors() {
            assert_eq!(encode(b""), "");
            assert_eq!(encode(b"f"), "Zg==");
            assert_eq!(encode(b"fo"), "Zm8=");
            assert_eq!(encode(b"foo"), "Zm9v");
            assert_eq!(encode(b"foob"), "Zm9vYg==");
            assert_eq!(encode(b"fooba"), "Zm9vYmE=");
            assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        }

        #[test]
        fn all_byte_values_round_trip_shape() {
            let all: Vec<u8> = (0..=255).collect();
            let enc = encode(&all);
            assert_eq!(enc.len(), 344); // ceil(256/3)*4
            assert!(enc.ends_with("=="));
            assert!(enc
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcu_core::{
        CoordSpace, DesktopGeometry, MockCapture, MockInput, MockWindow, MockClock, Timing,
    };

    fn identity_space() -> CoordSpace {
        CoordSpace {
            image_w: 1920,
            image_h: 1080,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        }
    }

    fn executor() -> Executor<MockCapture, MockInput, MockWindow, MockClock> {
        Executor::new(
            MockCapture::new(identity_space()),
            MockInput::new(),
            MockWindow::default(),
            MockClock::new(),
            Timing::default(),
            DesktopGeometry::single(1920.0, 1080.0),
        )
    }

    fn text_of(c: &Content) -> &str {
        match c {
            Content::Text { text } => text,
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn unknown_tool_is_a_transport_error() {
        let mut ex = executor();
        let r = call_tool(&mut ex, "click", &serde_json::json!({}), "image/png");
        assert!(r.is_error);
        assert!(text_of(&r.content[0]).contains("unknown tool"));
    }

    #[test]
    fn malformed_arguments_are_a_transport_error() {
        let mut ex = executor();
        let r = call_tool(
            &mut ex,
            TOOL_NAME,
            &serde_json::json!({"actions": [{"type": "click"}]}), // missing fields
            "image/png",
        );
        assert!(r.is_error);
        assert!(text_of(&r.content[0]).contains("invalid arguments"));
    }

    #[test]
    fn screenshot_batch_yields_desc_and_image_content() {
        let mut ex = executor();
        let r = call_tool(
            &mut ex,
            TOOL_NAME,
            &serde_json::json!({"actions": [
                {"type": "screenshot", "note": null},
                {"type": "click", "frame": 1, "button": "left", "x": 960, "y": 540},
            ]}),
            "image/png",
        );
        assert!(!r.is_error);
        assert_eq!(r.content.len(), 4);

        // 1: screenshot status line
        let status: serde_json::Value = serde_json::from_str(text_of(&r.content[0])).unwrap();
        assert_eq!(status["action"], "screenshot");
        assert_eq!(status["status"], "ok");

        // 2: self-describing frame payload the model binds coords to
        let desc: serde_json::Value = serde_json::from_str(text_of(&r.content[1])).unwrap();
        assert_eq!(desc["frame_id"], 1);
        assert_eq!(desc["width"], 1920);

        // 3: image bytes, base64, caller's mime type
        match &r.content[2] {
            Content::Image { data, mime_type } => {
                assert_eq!(mime_type, "image/png");
                let expected = base64::encode(ex.screenshot_bytes(FrameId(1)).unwrap());
                assert_eq!(data, &expected);
            }
            _ => panic!("expected image content"),
        }

        // 4: click status line with mapped desktop coords
        let click: serde_json::Value = serde_json::from_str(text_of(&r.content[3])).unwrap();
        assert_eq!(click["action"], "click");
        assert_eq!(click["status"], "ok");
        assert!((click["mapped"][0]["x"].as_f64().unwrap() - 960.0).abs() < 1e-9);
    }

    #[test]
    fn failed_action_reports_status_without_failing_call() {
        let mut ex = executor();
        let r = call_tool(
            &mut ex,
            TOOL_NAME,
            &serde_json::json!({"actions": [
                {"type": "click", "frame": 99, "button": "left", "x": 10, "y": 10},
                {"type": "wait", "ms": 5},
            ]}),
            "image/png",
        );
        assert!(!r.is_error); // partial success, not a transport failure
        assert_eq!(r.content.len(), 2);
        let failed: serde_json::Value = serde_json::from_str(text_of(&r.content[0])).unwrap();
        assert_eq!(failed["status"], "error");
        assert!(failed["error"].as_str().unwrap().contains("frame#99"));
        let noop: serde_json::Value = serde_json::from_str(text_of(&r.content[1])).unwrap();
        assert_eq!(noop["status"], "noop"); // batch continued
    }

    #[test]
    fn tools_list_exposes_single_batched_tool() {
        let list = tools_list();
        let tools = list["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], TOOL_NAME);
        let schema = &tools[0]["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["actions"]));
        // Spot-check: the drag entry carries a path array.
        let items = schema["properties"]["actions"]["items"]["oneOf"]
            .as_array()
            .unwrap();
        assert!(items.iter().any(|s| s["properties"]["path"]["type"] == "array"));
    }

    #[test]
    fn call_tool_result_serializes_to_mcp_shape() {
        let r = CallToolResult {
            content: vec![Content::text("hi"), Content::image("AA==", "image/png")],
            is_error: false,
        };
        let v = r.to_json();
        assert_eq!(v["isError"], false);
        assert_eq!(v["content"][0], serde_json::json!({"type": "text", "text": "hi"}));
        assert_eq!(
            v["content"][1],
            serde_json::json!({"type": "image", "data": "AA==", "mimeType": "image/png"})
        );
    }
}
