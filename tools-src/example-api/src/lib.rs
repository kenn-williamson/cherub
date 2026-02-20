//! Example WASM tool: fetches JSON data from JSONPlaceholder.
//!
//! Demonstrates:
//! - Using `wit_bindgen::generate!` for the guest side.
//! - Calling `http_request` host function.
//! - Calling `log` host function for progress reporting.
//! - Returning structured JSON output.
//!
//! # Capabilities required
//!
//! See `example-api.capabilities.toml`:
//! - HTTP: `jsonplaceholder.typicode.com`
//! - No credentials (public API)
//!
//! # Example invocation
//!
//! ```json
//! {"action": "get_post", "id": 1}
//! {"action": "list_posts", "limit": 5}
//! {"action": "get_user", "id": 1}
//! ```

wit_bindgen::generate!({
    world: "sandboxed-tool",
    path: "../../wit/tool.wit",
});

use exports::cherub::sandbox::tool::{Guest, Request, Response};
use serde::{Deserialize, Serialize};

struct ExampleApiTool;

impl Guest for ExampleApiTool {
    fn execute(req: Request) -> Response {
        crate::cherub::sandbox::host::log(
            crate::cherub::sandbox::host::LogLevel::Info,
            format!("example-api: executing with params: {}", req.params),
        );

        match execute_inner(&req.params) {
            Ok(output) => Response {
                output: Some(output),
                error: None,
            },
            Err(e) => Response {
                output: None,
                error: Some(e),
            },
        }
    }

    fn schema() -> String {
        r#"{
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["get_post", "list_posts", "get_user"],
                    "description": "Operation to perform"
                },
                "id": {
                    "type": "integer",
                    "description": "Resource ID (required for get_post, get_user)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (used by list_posts, default: 10)",
                    "default": 10
                }
            }
        }"#
        .to_string()
    }

    fn description() -> String {
        "Fetch example JSON data from JSONPlaceholder (public test API). \
         Supports listing posts, getting a specific post by ID, and fetching user info."
            .to_string()
    }
}

// ─── Parameter types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    GetPost { id: u64 },
    ListPosts { limit: Option<u64> },
    GetUser { id: u64 },
}

// ─── Execution ────────────────────────────────────────────────────────────────

fn execute_inner(params: &str) -> Result<String, String> {
    let action: Action =
        serde_json::from_str(params).map_err(|e| format!("invalid parameters: {e}"))?;

    match action {
        Action::GetPost { id } => get_post(id),
        Action::ListPosts { limit } => list_posts(limit.unwrap_or(10)),
        Action::GetUser { id } => get_user(id),
    }
}

fn get_post(id: u64) -> Result<String, String> {
    use crate::cherub::sandbox::host::{LogLevel, log, http_request};

    log(LogLevel::Info, format!("fetching post {id}"));

    let response = http_request(
        "GET".to_string(),
        format!("https://jsonplaceholder.typicode.com/posts/{id}"),
        "{}".to_string(),
        None,
        Some(10_000),
    )
    .map_err(|e| format!("HTTP request failed: {e}"))?;

    if response.status != 200 {
        return Err(format!("unexpected status {}", response.status));
    }

    let body = String::from_utf8_lossy(&response.body).into_owned();
    log(LogLevel::Info, format!("got post {id}: {} bytes", body.len()));
    Ok(body)
}

fn list_posts(limit: u64) -> Result<String, String> {
    use crate::cherub::sandbox::host::{LogLevel, log, http_request};

    log(LogLevel::Info, format!("listing up to {limit} posts"));

    let response = http_request(
        "GET".to_string(),
        "https://jsonplaceholder.typicode.com/posts".to_string(),
        "{}".to_string(),
        None,
        Some(15_000),
    )
    .map_err(|e| format!("HTTP request failed: {e}"))?;

    if response.status != 200 {
        return Err(format!("unexpected status {}", response.status));
    }

    let body = String::from_utf8_lossy(&response.body);
    let posts: Vec<serde_json::Value> =
        serde_json::from_str(&body).map_err(|e| format!("invalid JSON: {e}"))?;

    let limited: Vec<_> = posts.into_iter().take(limit as usize).collect();
    serde_json::to_string(&limited).map_err(|e| format!("serialization failed: {e}"))
}

fn get_user(id: u64) -> Result<String, String> {
    use crate::cherub::sandbox::host::{LogLevel, log, http_request};

    log(LogLevel::Info, format!("fetching user {id}"));

    let response = http_request(
        "GET".to_string(),
        format!("https://jsonplaceholder.typicode.com/users/{id}"),
        "{}".to_string(),
        None,
        Some(10_000),
    )
    .map_err(|e| format!("HTTP request failed: {e}"))?;

    if response.status != 200 {
        return Err(format!("unexpected status {}", response.status));
    }

    String::from_utf8(response.body).map_err(|_| "response is not valid UTF-8".to_string())
}

export!(ExampleApiTool);
