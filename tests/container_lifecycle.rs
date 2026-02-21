//! Container IPC protocol interop tests (M9c).
//!
//! Tests the full IPC stack — wire format, Registration/Execute/Result flow,
//! Log message passthrough, and Shutdown — using a real Python subprocess as
//! the "container". No Docker daemon required; these run wherever Python 3 is
//! available.
//!
//! The tests are skipped automatically if `python3` is not found on PATH.
//!
//! Run:
//!   cargo test --features container --test container_lifecycle

#![cfg(feature = "container")]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use cherub::enforcement::{self, tier::Tier};
use cherub::error::CherubError;
use cherub::tools::ToolContext;
use cherub::tools::container::ipc::{IpcTransport, RuntimeMessage, ToolMessage};
use cherub::tools::container::{
    ContainerCapabilities, ContainerConfig, ContainerRuntime, ContainerTool, ContainerToolMetadata,
};
use uuid::Uuid;

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Returns true if `python3` is available on PATH.
fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Write a Python script to a temp file and return the path.
fn write_script(dir: &TempDir, name: &str, source: &str) -> PathBuf {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).expect("create script");
    f.write_all(source.as_bytes()).expect("write script");
    path
}

/// A minimal IPC server script: sends Registration, handles Execute, sends Result,
/// handles Shutdown.
const MINIMAL_TOOL_SCRIPT: &str = r#"
import json, os, struct, socket, sys

IPC_SOCKET_PATH = os.environ.get("CHERUB_IPC_SOCKET", "/ipc/tool.sock")
MAX_FRAME = 16 * 1024 * 1024

def send_frame(s, payload):
    s.sendall(struct.pack(">I", len(payload)) + payload)

def recv_frame(s):
    buf = b""
    while len(buf) < 4:
        chunk = s.recv(4 - len(buf))
        if not chunk: raise EOFError
        buf += chunk
    n = struct.unpack(">I", buf)[0]
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk: raise EOFError
        buf += chunk
    return buf

def send_msg(s, d):  send_frame(s, json.dumps(d).encode())
def recv_msg(s): return json.loads(recv_frame(s))

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(IPC_SOCKET_PATH)

# Registration
send_msg(sock, {"type": "registration", "name": "test-tool",
    "description": "A test tool", "schema": {"type": "object"}})

while True:
    msg = recv_msg(sock)
    if msg["type"] == "execute":
        # Echo the params back as output, plus a log message
        send_msg(sock, {"type": "log", "level": "info", "message": "processing call"})
        send_msg(sock, {"type": "result", "id": msg["id"],
            "output": f"echo: {json.dumps(msg['params'])}"})
    elif msg["type"] == "shutdown":
        break

sock.close()
"#;

/// A tool script that returns an error for any call.
const ERROR_TOOL_SCRIPT: &str = r#"
import json, struct, socket, os

IPC_SOCKET_PATH = os.environ.get("CHERUB_IPC_SOCKET", "/ipc/tool.sock")

def send_frame(s, p): s.sendall(struct.pack(">I", len(p)) + p)
def recv_frame(s):
    b = b""
    while len(b) < 4:
        c = s.recv(4 - len(b));
        if not c: raise EOFError
        b += c
    n = struct.unpack(">I", b)[0]; b = b""
    while len(b) < n:
        c = s.recv(n - len(b));
        if not c: raise EOFError
        b += c
    return b
def send_msg(s, d): send_frame(s, json.dumps(d).encode())
def recv_msg(s): return json.loads(recv_frame(s))

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(IPC_SOCKET_PATH)
send_msg(sock, {"type": "registration", "name": "error-tool",
    "description": "Always fails", "schema": {"type": "object"}})
while True:
    msg = recv_msg(sock)
    if msg["type"] == "execute":
        send_msg(sock, {"type": "result", "id": msg["id"], "error": "intentional tool error"})
    elif msg["type"] == "shutdown":
        break
sock.close()
"#;

// ─── Mock ContainerRuntime ────────────────────────────────────────────────────

/// A mock `ContainerRuntime` that spawns a Python subprocess instead of Docker.
///
/// The subprocess script path is embedded at construction time; the IPC
/// socket path is injected via `CHERUB_IPC_SOCKET` env var.
struct PythonMockRuntime {
    script: PathBuf,
    /// Tracks spawned child PIDs (key = fake container_id = PID as string).
    children: tokio::sync::Mutex<Vec<tokio::process::Child>>,
}

impl PythonMockRuntime {
    fn new(script: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            script,
            children: tokio::sync::Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ContainerRuntime for PythonMockRuntime {
    async fn is_available(&self) -> bool {
        python3_available()
    }

    async fn spawn(&self, config: &ContainerConfig) -> Result<String, CherubError> {
        let socket_path = config.ipc_dir.join("tool.sock");
        let child = Command::new("python3")
            .arg(&self.script)
            .env("CHERUB_IPC_SOCKET", &socket_path)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| CherubError::Container(format!("python3 spawn failed: {e}")))?;
        let pid = child.id().unwrap_or(0).to_string();
        self.children.lock().await.push(child);
        Ok(pid)
    }

    async fn stop(&self, _container_id: &str) -> Result<(), CherubError> {
        // kill_on_drop handles cleanup when the child is dropped.
        Ok(())
    }

    async fn remove(&self, _container_id: &str) -> Result<(), CherubError> {
        Ok(())
    }

    async fn is_running(&self, _container_id: &str) -> Result<bool, CherubError> {
        // For simplicity, assume always running during a test.
        Ok(true)
    }
}

// ─── IPC protocol interop tests ───────────────────────────────────────────────

/// Send Execute → receive Log + Result, check echo output.
#[tokio::test]
async fn ipc_execute_echo_roundtrip() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let script = write_script(&tmp, "tool.py", MINIMAL_TOOL_SCRIPT);

    // Bind the listener.
    let sock_path = tmp.path().join("tool.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind");

    // Spawn Python subprocess.
    let mut child = Command::new("python3")
        .arg(&script)
        .env("CHERUB_IPC_SOCKET", &sock_path)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn python3");

    // Accept the connection.
    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("accept timeout")
        .expect("accept");
    let mut transport = IpcTransport::new(stream);

    // Expect Registration.
    let msg = timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("Registration timeout")
        .expect("recv Registration");
    match msg {
        ToolMessage::Registration { name, .. } => assert_eq!(name, "test-tool"),
        other => panic!("expected Registration, got {other:?}"),
    }

    // Send Execute.
    transport
        .send(&RuntimeMessage::Execute {
            id: 1,
            params: serde_json::json!({"key": "value"}),
            context: None,
        })
        .await
        .expect("send Execute");

    // Expect Log then Result.
    let log_msg = timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("Log timeout")
        .expect("recv Log");
    match log_msg {
        ToolMessage::Log { level, message } => {
            assert_eq!(level, "info");
            assert!(message.contains("processing"), "got: {message}");
        }
        other => panic!("expected Log, got {other:?}"),
    }

    let result_msg = timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("Result timeout")
        .expect("recv Result");
    match result_msg {
        ToolMessage::Result { id, output, error } => {
            assert_eq!(id, 1);
            assert!(error.is_none(), "unexpected error: {error:?}");
            let out = output.expect("expected output");
            assert!(out.contains("echo:"), "output was: {out}");
            assert!(out.contains("key"), "output was: {out}");
        }
        other => panic!("expected Result, got {other:?}"),
    }

    // Send Shutdown.
    transport
        .send(&RuntimeMessage::Shutdown)
        .await
        .expect("send Shutdown");

    // Python should exit cleanly.
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit timeout")
        .expect("wait");
    assert!(status.success(), "python3 exited with: {status}");
}

/// Error result from tool is propagated.
#[tokio::test]
async fn ipc_error_result_propagated() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let script = write_script(&tmp, "error_tool.py", ERROR_TOOL_SCRIPT);
    let sock_path = tmp.path().join("tool.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind");

    let mut child = Command::new("python3")
        .arg(&script)
        .env("CHERUB_IPC_SOCKET", &sock_path)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");

    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("accept timeout")
        .expect("accept");
    let mut transport = IpcTransport::new(stream);

    // Consume Registration.
    timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("Registration timeout")
        .expect("recv");

    transport
        .send(&RuntimeMessage::Execute {
            id: 99,
            params: serde_json::json!({}),
            context: None,
        })
        .await
        .expect("send");

    let result = timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("timeout")
        .expect("recv");
    match result {
        ToolMessage::Result { id, output, error } => {
            assert_eq!(id, 99);
            assert!(output.is_none());
            let err = error.expect("expected error");
            assert!(err.contains("intentional"), "got: {err}");
        }
        other => panic!("expected Result, got {other:?}"),
    }

    transport
        .send(&RuntimeMessage::Shutdown)
        .await
        .expect("shutdown");
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

// ─── ContainerTool integration (Python mock runtime) ──────────────────────────

/// Full ContainerTool execute() lifecycle via Python subprocess mock.
#[tokio::test]
async fn container_tool_execute_via_mock_runtime() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let ipc_dir = tmp.path().join("ipc");
    std::fs::create_dir_all(&ipc_dir).expect("mkdir ipc");

    let script = write_script(&tmp, "tool.py", MINIMAL_TOOL_SCRIPT);
    let runtime = PythonMockRuntime::new(script);

    let metadata = ContainerToolMetadata {
        name: "test-tool".to_owned(),
        description: "A test tool".to_owned(),
        image: "unused-image:latest".to_owned(),
        schema: serde_json::json!({"type": "object"}),
    };

    let caps = ContainerCapabilities::default();
    let tool = ContainerTool::new(metadata, runtime, caps, ipc_dir);

    // Build a CapabilityToken via approve_escalation — the public enforcement API
    // for creating tokens in test/approval contexts.
    let token = enforcement::approve_escalation(Tier::Observe);
    let ctx = ToolContext {
        user_id: "test-user".to_owned(),
        session_id: Uuid::now_v7(),
        turn_number: 1,
    };

    let result = timeout(
        Duration::from_secs(30),
        tool.execute(&serde_json::json!({"hello": "world"}), token, &ctx),
    )
    .await
    .expect("execute timeout")
    .expect("execute");

    assert!(
        result.output.contains("echo:"),
        "output was: {:?}",
        result.output
    );
    assert!(
        result.output.contains("hello"),
        "output was: {:?}",
        result.output
    );

    tool.shutdown().await;
}

// ─── Docker integration (requires Docker daemon + pre-built image) ─────────────

/// End-to-end test using the real text-analysis Docker image.
///
/// Requires:
///   1. Docker daemon running
///   2. Image built: `docker build -t cherub-tool-text-analysis:latest tools/container/text-analysis/`
///
/// Run with:
///   cargo nextest run --features container --test container_lifecycle -- --ignored
#[tokio::test]
#[ignore]
async fn container_tool_docker_text_analysis() {
    use cherub::tools::container::BollardRuntime;

    let runtime = match BollardRuntime::new() {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("skipping: Docker not available: {e}");
            return;
        }
    };

    if !runtime.is_available().await {
        eprintln!("skipping: Docker daemon not reachable");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let ipc_dir = tmp.path().join("ipc");
    std::fs::create_dir_all(&ipc_dir).expect("mkdir ipc");

    let metadata = ContainerToolMetadata {
        name: "text-analysis".to_owned(),
        description: "Analyze text.".to_owned(),
        image: "cherub-tool-text-analysis:latest".to_owned(),
        schema: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
    };
    let caps = ContainerCapabilities::default();
    let tool = ContainerTool::new(metadata, runtime, caps, ipc_dir);

    let token = enforcement::approve_escalation(Tier::Observe);
    let ctx = ToolContext {
        user_id: "test-user".to_owned(),
        session_id: Uuid::now_v7(),
        turn_number: 1,
    };

    let result = timeout(
        Duration::from_secs(120),
        tool.execute(
            &serde_json::json!({"text": "Hello world. This is a test sentence!"}),
            token,
            &ctx,
        ),
    )
    .await
    .expect("execute timeout")
    .expect("execute");

    let out = &result.output;
    assert!(out.contains("Words"), "output: {out}");
    assert!(
        out.contains("3"),
        "expected 3 words (Hello world This), output: {out}"
    );

    tool.shutdown().await;
}
