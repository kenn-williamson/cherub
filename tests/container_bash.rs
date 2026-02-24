//! Container-sandboxed bash tool tests.
//!
//! Tests the sandbox bash factory, IPC client output format (via Python
//! subprocess mock), and registry wiring.
//!
//! Run:
//!   cargo nextest run --features container --test container_bash

#![cfg(feature = "container")]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use cherub::error::CherubError;
use cherub::tools::ToolRegistry;
use cherub::tools::container::ipc::{IpcTransport, RuntimeMessage, ToolMessage};
use cherub::tools::container::{ContainerConfig, ContainerRuntime};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Path to the real ipc_client.py used by the sandbox bash container.
fn ipc_client_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent dir")
        .join("tools/container/sandbox-bash/ipc_client.py")
}

/// Mock container runtime that spawns a Python subprocess.
struct PythonMockRuntime {
    script: PathBuf,
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
        Ok(())
    }

    async fn remove(&self, _container_id: &str) -> Result<(), CherubError> {
        Ok(())
    }

    async fn is_running(&self, _container_id: &str) -> Result<bool, CherubError> {
        Ok(true)
    }
}

// ─── IPC client output format tests ──────────────────────────────────────────

/// Verify the IPC client echoes stdout correctly.
#[tokio::test]
async fn ipc_client_echo() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let sock_path = tmp.path().join("tool.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind");

    let mut child = Command::new("python3")
        .arg(ipc_client_path())
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
    let reg = timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("timeout")
        .expect("recv");
    match reg {
        ToolMessage::Registration { name, .. } => assert_eq!(name, "bash"),
        other => panic!("expected Registration, got {other:?}"),
    }

    // Send Execute: echo hello
    transport
        .send(&RuntimeMessage::Execute {
            id: 1,
            params: serde_json::json!({"command": "echo hello"}),
            context: None,
        })
        .await
        .expect("send");

    let result = timeout(Duration::from_secs(10), transport.recv())
        .await
        .expect("timeout")
        .expect("recv");
    match result {
        ToolMessage::Result { id, output, error } => {
            assert_eq!(id, 1);
            assert!(error.is_none());
            let out = output.expect("expected output");
            assert_eq!(out.trim(), "hello");
        }
        other => panic!("expected Result, got {other:?}"),
    }

    transport
        .send(&RuntimeMessage::Shutdown)
        .await
        .expect("shutdown");
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

/// Verify stderr is appended to stdout (same as BashTool).
#[tokio::test]
async fn ipc_client_stderr() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let sock_path = tmp.path().join("tool.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind");

    let mut child = Command::new("python3")
        .arg(ipc_client_path())
        .env("CHERUB_IPC_SOCKET", &sock_path)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");

    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("accept timeout")
        .expect("accept");
    let mut transport = IpcTransport::new(stream);

    timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("timeout")
        .expect("recv Registration");

    transport
        .send(&RuntimeMessage::Execute {
            id: 2,
            params: serde_json::json!({"command": "echo out && echo err >&2"}),
            context: None,
        })
        .await
        .expect("send");

    let result = timeout(Duration::from_secs(10), transport.recv())
        .await
        .expect("timeout")
        .expect("recv");
    match result {
        ToolMessage::Result { id, output, error } => {
            assert_eq!(id, 2);
            assert!(error.is_none());
            let out = output.expect("expected output");
            assert!(out.contains("out"), "output was: {out}");
            assert!(out.contains("err"), "output was: {out}");
        }
        other => panic!("expected Result, got {other:?}"),
    }

    transport
        .send(&RuntimeMessage::Shutdown)
        .await
        .expect("shutdown");
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

/// Verify non-zero exit code is formatted as "[exit code: N]".
#[tokio::test]
async fn ipc_client_exit_code() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let sock_path = tmp.path().join("tool.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind");

    let mut child = Command::new("python3")
        .arg(ipc_client_path())
        .env("CHERUB_IPC_SOCKET", &sock_path)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");

    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("accept timeout")
        .expect("accept");
    let mut transport = IpcTransport::new(stream);

    timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("timeout")
        .expect("recv Registration");

    transport
        .send(&RuntimeMessage::Execute {
            id: 3,
            params: serde_json::json!({"command": "exit 42"}),
            context: None,
        })
        .await
        .expect("send");

    let result = timeout(Duration::from_secs(10), transport.recv())
        .await
        .expect("timeout")
        .expect("recv");
    match result {
        ToolMessage::Result { id, output, error } => {
            assert_eq!(id, 3);
            assert!(error.is_none());
            let out = output.expect("expected output");
            assert!(out.contains("[exit code: 42]"), "output was: {out}");
        }
        other => panic!("expected Result, got {other:?}"),
    }

    transport
        .send(&RuntimeMessage::Shutdown)
        .await
        .expect("shutdown");
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

/// Verify output truncation at 256 KiB.
#[tokio::test]
async fn ipc_client_truncation() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let sock_path = tmp.path().join("tool.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind");

    let mut child = Command::new("python3")
        .arg(ipc_client_path())
        .env("CHERUB_IPC_SOCKET", &sock_path)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");

    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("accept timeout")
        .expect("accept");
    let mut transport = IpcTransport::new(stream);

    timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("timeout")
        .expect("recv Registration");

    // Generate output larger than 256 KiB.
    transport
        .send(&RuntimeMessage::Execute {
            id: 4,
            params: serde_json::json!({"command": "head -c 300000 /dev/urandom | base64"}),
            context: None,
        })
        .await
        .expect("send");

    let result = timeout(Duration::from_secs(30), transport.recv())
        .await
        .expect("timeout")
        .expect("recv");
    match result {
        ToolMessage::Result { id, output, error } => {
            assert_eq!(id, 4);
            assert!(error.is_none());
            let out = output.expect("expected output");
            assert!(
                out.contains("[output truncated]"),
                "output was: ...{}",
                &out[out.len().saturating_sub(50)..]
            );
        }
        other => panic!("expected Result, got {other:?}"),
    }

    transport
        .send(&RuntimeMessage::Shutdown)
        .await
        .expect("shutdown");
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

/// Verify missing command parameter returns error.
#[tokio::test]
async fn ipc_client_missing_command() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let sock_path = tmp.path().join("tool.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind");

    let mut child = Command::new("python3")
        .arg(ipc_client_path())
        .env("CHERUB_IPC_SOCKET", &sock_path)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");

    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("accept timeout")
        .expect("accept");
    let mut transport = IpcTransport::new(stream);

    timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("timeout")
        .expect("recv Registration");

    transport
        .send(&RuntimeMessage::Execute {
            id: 5,
            params: serde_json::json!({"args": ["--version"]}),
            context: None,
        })
        .await
        .expect("send");

    let result = timeout(Duration::from_secs(10), transport.recv())
        .await
        .expect("timeout")
        .expect("recv");
    match result {
        ToolMessage::Result { id, output, error } => {
            assert_eq!(id, 5);
            assert!(output.is_none());
            let err = error.expect("expected error");
            assert!(err.contains("command"), "error was: {err}");
        }
        other => panic!("expected Result, got {other:?}"),
    }

    transport
        .send(&RuntimeMessage::Shutdown)
        .await
        .expect("shutdown");
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

// ─── Registry tests ──────────────────────────────────────────────────────────

/// new_without_bash() + with_container(bash) registers exactly one tool.
#[tokio::test]
async fn registry_no_bash_then_container_bash() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let runtime = PythonMockRuntime::new(ipc_client_path());
    let workspace = std::env::current_dir().expect("cwd");
    let (bash_tool, _ipc_dir) = cherub::tools::container_bash::build(runtime, workspace);

    let registry = ToolRegistry::new_without_bash().with_container(vec![bash_tool]);

    // Exactly one tool should be registered (the container bash, no duplicate built-in).
    let defs = registry.definitions();
    assert_eq!(defs.len(), 1, "expected exactly 1 tool, got {}", defs.len());
}

/// Regular new() has 1 tool (bash); new_without_bash() has 0.
#[test]
fn registry_new_vs_no_bash() {
    let with = ToolRegistry::new();
    assert_eq!(with.definitions().len(), 1);

    let without = ToolRegistry::new_without_bash();
    assert_eq!(without.definitions().len(), 0);
}

// ─── Docker integration (requires Docker + built image) ─────────────────────

/// End-to-end test using the real sandbox-bash Docker image.
///
/// Requires:
///   1. Docker daemon running
///   2. Image built: `docker build -t cherub-sandbox-bash:latest tools/container/sandbox-bash/`
///
/// Run with:
///   cargo nextest run --features container --test container_bash -- --ignored
#[tokio::test]
#[ignore]
async fn container_bash_docker_e2e() {
    use cherub::enforcement::{self, tier::Tier};
    use cherub::tools::ToolContext;
    use cherub::tools::container::BollardRuntime;
    use uuid::Uuid;

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

    let workspace = std::env::current_dir().expect("cwd");
    let (bash_tool, _ipc_dir) =
        cherub::tools::container_bash::build(runtime as Arc<dyn ContainerRuntime>, workspace);

    let token = enforcement::approve_escalation(Tier::Observe);
    let ctx = ToolContext {
        user_id: "test-user".to_owned(),
        session_id: Uuid::now_v7(),
        turn_number: 1,
    };

    // Test 1: echo hello
    let result = timeout(
        Duration::from_secs(120),
        bash_tool.execute(&serde_json::json!({"command": "echo hello"}), token, &ctx),
    )
    .await
    .expect("execute timeout")
    .expect("execute");
    assert_eq!(result.output.trim(), "hello");

    // Test 2: env should only show container-internal vars (not host secrets)
    let token2 = enforcement::approve_escalation(Tier::Observe);
    let result2 = timeout(
        Duration::from_secs(30),
        bash_tool.execute(&serde_json::json!({"command": "env"}), token2, &ctx),
    )
    .await
    .expect("execute timeout")
    .expect("execute");
    assert!(
        result2.output.contains("CHERUB_IPC_SOCKET"),
        "env should show IPC socket: {}",
        result2.output
    );
    assert!(
        !result2.output.contains("ANTHROPIC_API_KEY"),
        "env should NOT show API key: {}",
        result2.output
    );
    assert!(
        !result2.output.contains("CHERUB_MASTER_KEY"),
        "env should NOT show master key: {}",
        result2.output
    );
    assert!(
        !result2.output.contains("DATABASE_URL"),
        "env should NOT show database URL: {}",
        result2.output
    );

    // Test 3: policy file should not exist
    let token3 = enforcement::approve_escalation(Tier::Observe);
    let result3 = timeout(
        Duration::from_secs(30),
        bash_tool.execute(
            &serde_json::json!({"command": "cat config/default_policy.toml 2>&1"}),
            token3,
            &ctx,
        ),
    )
    .await
    .expect("execute timeout")
    .expect("execute");
    assert!(
        result3.output.contains("No such file"),
        "policy file should not be accessible: {}",
        result3.output
    );

    // Test 4: workspace is visible (Cargo.toml should exist)
    let token4 = enforcement::approve_escalation(Tier::Observe);
    let result4 = timeout(
        Duration::from_secs(30),
        bash_tool.execute(
            &serde_json::json!({"command": "ls Cargo.toml"}),
            token4,
            &ctx,
        ),
    )
    .await
    .expect("execute timeout")
    .expect("execute");
    assert!(
        result4.output.contains("Cargo.toml"),
        "workspace should be visible: {}",
        result4.output
    );

    // Test 5: host filesystem not visible (host /etc/hostname differs from container)
    let token5 = enforcement::approve_escalation(Tier::Observe);
    let result5 = timeout(
        Duration::from_secs(30),
        bash_tool.execute(
            &serde_json::json!({"command": "cat /etc/os-release 2>&1 | head -1"}),
            token5,
            &ctx,
        ),
    )
    .await
    .expect("execute timeout")
    .expect("execute");
    // Container uses Debian (python:3.13-slim), not the host OS.
    assert!(
        result5.output.contains("Debian") || result5.output.contains("debian"),
        "container should run Debian, got: {}",
        result5.output
    );

    // Test 6: build tools work (if image built with LANGUAGES=rust)
    let token6 = enforcement::approve_escalation(Tier::Observe);
    let result6 = timeout(
        Duration::from_secs(30),
        bash_tool.execute(
            &serde_json::json!({"command": "which cargo 2>/dev/null && cargo --version || echo 'no cargo'"}),
            token6,
            &ctx,
        ),
    )
    .await
    .expect("execute timeout")
    .expect("execute");
    // This test passes whether or not Rust was installed — it just logs the result.
    eprintln!("cargo availability: {}", result6.output.trim());

    bash_tool.shutdown().await;
}
