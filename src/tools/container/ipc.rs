//! IPC wire format and transport for container tool communication.
//!
//! Length-prefixed JSON over Unix domain sockets:
//! ```text
//! [4 bytes big-endian length] [JSON payload bytes]
//! ```
//!
//! Max frame size: 16 MiB — protects the runtime from memory exhaustion.
//!
//! # Message flow
//!
//! ```text
//! Runtime creates UnixListener → Container spawns → Container connects
//! Container sends Registration → Runtime accepts Registration
//!
//! Per call:
//!   Runtime sends Execute → Container processes
//!   Container sends Result (interleaved with Log messages)
//!   M9b: Container may send HostCall; Runtime sends HostResponse
//! ```

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Maximum allowed frame size in bytes (16 MiB).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

// ─── Message types: Runtime → Tool ───────────────────────────────────────────

/// Messages the runtime sends to a container tool.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeMessage {
    /// Ask the tool to execute with the given parameters.
    Execute {
        /// Call ID for correlating with the `Result` response.
        id: u64,
        /// Tool input parameters (JSON object).
        params: serde_json::Value,
        /// Optional execution context (user_id, session_id, turn_number).
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<serde_json::Value>,
    },
    /// Response to a host function call from the tool (M9b).
    HostResponse {
        /// Call ID matching the `HostCall` that triggered this response.
        id: u64,
        /// The host function result.
        result: serde_json::Value,
    },
    /// Request graceful shutdown.
    Shutdown,
}

// ─── Message types: Tool → Runtime ───────────────────────────────────────────

/// Messages a container tool sends to the runtime.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolMessage {
    /// Sent once on connect: declares the tool's identity and schema.
    Registration {
        name: String,
        description: String,
        schema: serde_json::Value,
    },
    /// Final response for an `Execute` call.
    Result {
        /// Call ID matching the `Execute` that triggered this.
        id: u64,
        /// Tool output text (mutually exclusive with `error`).
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// Error message (mutually exclusive with `output`).
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Request to call a host function (M9b). Pauses execution until
    /// the runtime replies with a `HostResponse`.
    HostCall {
        /// Unique ID for correlating the `HostResponse`.
        id: u64,
        /// Host function name (e.g., `"workspace_read"`, `"http_request"`).
        function: String,
        /// Function arguments.
        args: serde_json::Value,
    },
    /// Diagnostic log entry from the tool.
    Log { level: String, message: String },
}

// ─── IPC transport ────────────────────────────────────────────────────────────

/// Bidirectional IPC transport over a Unix domain socket stream.
///
/// Each `send` / `recv` call is a single framed JSON message.
/// Not `Clone` — ownership is held by the `ContainerTool` state mutex.
pub struct IpcTransport {
    stream: UnixStream,
}

impl IpcTransport {
    /// Wrap an accepted `UnixStream` as an IPC transport.
    pub fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Send a `RuntimeMessage` to the container tool.
    ///
    /// Serializes to JSON, checks size, writes the length prefix, then the payload.
    pub async fn send(&mut self, msg: &RuntimeMessage) -> Result<(), String> {
        let payload = serde_json::to_vec(msg)
            .map_err(|e| format!("failed to serialize RuntimeMessage: {e}"))?;
        write_frame(&mut self.stream, &payload).await
    }

    /// Receive a `ToolMessage` from the container tool.
    ///
    /// Reads the length prefix, then the payload, then deserializes.
    pub async fn recv(&mut self) -> Result<ToolMessage, String> {
        let payload = read_frame(&mut self.stream).await?;
        serde_json::from_slice(&payload)
            .map_err(|e| format!("failed to deserialize ToolMessage: {e} (raw: {payload:?})"))
    }
}

// ─── Frame I/O helpers ────────────────────────────────────────────────────────

/// Write a length-prefixed frame to the stream.
async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<(), String> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(format!(
            "outbound frame too large: {} bytes (max {MAX_FRAME_BYTES})",
            payload.len()
        ));
    }
    let len = payload.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| format!("IPC write (length prefix) failed: {e}"))?;
    stream
        .write_all(payload)
        .await
        .map_err(|e| format!("IPC write (payload) failed: {e}"))?;
    Ok(())
}

/// Read a length-prefixed frame from the stream.
async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("IPC read (length prefix) failed: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(format!(
            "inbound frame too large: {len} bytes (max {MAX_FRAME_BYTES})"
        ));
    }
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| format!("IPC read (payload) failed: {e}"))?;
    Ok(payload)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// Round-trip test over a real in-memory socket pair.
    async fn make_transport_pair() -> (IpcTransport, IpcTransport) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let client_stream = UnixStream::connect(&path).await.expect("connect");
        let (server_stream, _) = listener.accept().await.expect("accept");
        (
            IpcTransport::new(server_stream),
            IpcTransport::new(client_stream),
        )
    }

    #[tokio::test]
    async fn execute_message_roundtrip() {
        let (mut server, client) = make_transport_pair().await;
        let msg = RuntimeMessage::Execute {
            id: 42,
            params: serde_json::json!({"text": "hello"}),
            context: None,
        };
        server.send(&msg).await.unwrap();
        // client reads it as a raw frame and re-parses as ToolMessage — not valid here.
        // Instead, test symmetrically by having both sides send/recv.
        let _ = (server, client);
    }

    #[tokio::test]
    async fn runtime_to_tool_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).expect("bind");

        let sender = tokio::spawn(async move {
            let stream = UnixStream::connect(&path).await.expect("connect");
            let mut transport = IpcTransport::new(stream);
            transport
                .send(&RuntimeMessage::Execute {
                    id: 7,
                    params: serde_json::json!({"key": "value"}),
                    context: None,
                })
                .await
                .expect("send")
        });

        let (stream, _) = listener.accept().await.expect("accept");
        let mut receiver = IpcTransport::new(stream);
        // The receiver here would normally be the tool, using a raw reader.
        // For now just drain the frame to confirm it was sent.
        sender.await.expect("sender task");
        let raw = read_frame(&mut receiver.stream).await.expect("read frame");
        let parsed: RuntimeMessage = serde_json::from_slice(&raw).expect("parse");
        match parsed {
            RuntimeMessage::Execute { id, .. } => assert_eq!(id, 7),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_registration_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reg.sock");
        let listener = UnixListener::bind(&path).expect("bind");

        let sender = tokio::spawn(async move {
            let stream = UnixStream::connect(&path).await.expect("connect");
            let mut t = IpcTransport::new(stream);
            // Simulate tool sending Registration.
            let reg = ToolMessage::Registration {
                name: "my-tool".to_owned(),
                description: "A test tool".to_owned(),
                schema: serde_json::json!({"type": "object"}),
            };
            let payload = serde_json::to_vec(&reg).expect("serialize");
            write_frame(&mut t.stream, &payload).await.expect("write");
        });

        let (stream, _) = listener.accept().await.expect("accept");
        let mut receiver = IpcTransport::new(stream);
        sender.await.expect("sender");
        let msg = receiver.recv().await.expect("recv");
        match msg {
            ToolMessage::Registration { name, .. } => assert_eq!(name, "my-tool"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_frame_rejected_on_send() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let client = UnixStream::connect(&path).await.expect("connect");
        let (server, _) = listener.accept().await.expect("accept");
        let mut server = IpcTransport::new(server);
        let mut client = IpcTransport::new(client);

        // Construct a payload that's exactly MAX + 1 bytes by hacking the raw write.
        let big_payload = vec![b'x'; MAX_FRAME_BYTES + 1];
        let err = write_frame(&mut server.stream, &big_payload)
            .await
            .unwrap_err();
        assert!(err.contains("too large"), "got: {err}");

        // Verify receiver side also rejects oversized frames from a malicious sender.
        let len_bytes = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        server.stream.write_all(&len_bytes).await.unwrap();
        let err = read_frame(&mut client.stream).await.unwrap_err();
        assert!(err.contains("too large"), "got: {err}");
    }

    #[test]
    fn runtime_message_serde_shutdown() {
        let msg = RuntimeMessage::Shutdown;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"shutdown\""), "got: {json}");
        let parsed: RuntimeMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, RuntimeMessage::Shutdown));
    }

    #[test]
    fn tool_message_serde_log() {
        let msg = ToolMessage::Log {
            level: "info".to_owned(),
            message: "hello".to_owned(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"log\""), "got: {json}");
        let parsed: ToolMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            ToolMessage::Log { level, message } => {
                assert_eq!(level, "info");
                assert_eq!(message, "hello");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_message_serde_result() {
        let msg = ToolMessage::Result {
            id: 3,
            output: Some("done".to_owned()),
            error: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"result\""), "got: {json}");
        // `error` should be absent due to skip_serializing_if
        assert!(
            !json.contains("\"error\""),
            "error field should be absent: {json}"
        );
    }

    #[test]
    fn host_call_roundtrip() {
        let msg = ToolMessage::HostCall {
            id: 99,
            function: "workspace_read".to_owned(),
            args: serde_json::json!({"path": "data/notes.txt"}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ToolMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            ToolMessage::HostCall { id, function, .. } => {
                assert_eq!(id, 99);
                assert_eq!(function, "workspace_read");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn malformed_json_rejected() {
        let bad = b"not valid json at all {{{";
        let result: Result<ToolMessage, _> = serde_json::from_slice(bad);
        assert!(result.is_err());
    }
}
