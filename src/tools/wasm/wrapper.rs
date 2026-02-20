//! WASM component wrapper: bindgen bindings, StoreData, and WasmTool.
//!
//! Uses `wasmtime::component::bindgen!` to generate typed host/guest bindings
//! from `wit/tool.wit`. Each call to `WasmTool::execute()` creates a fresh
//! `Store<StoreData>` instance for isolation (NEAR pattern).

use std::sync::Arc;
use std::time::Duration;

use url::Url;
use wasmtime::Store;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::tools::ToolResult;
use crate::tools::wasm::host::{HostState, LogLevel, reject_private_ip};
use crate::tools::wasm::limits::WasmResourceLimiter;
use crate::tools::wasm::runtime::{PreparedModule, WasmToolRuntime};

// Generate component-model bindings from wit/tool.wit.
//
// This produces:
//   - `cherub::sandbox::host::Host` trait (implement on StoreData)
//   - `cherub::sandbox::host::add_to_linker()` for the host import interface
//   - `SandboxedTool::instantiate()` for the world
//   - `exports::cherub::sandbox::tool::*` for the guest exports
wasmtime::component::bindgen!({
    path: "wit/tool.wit",
    world: "sandboxed-tool",
    with: {},
});

// Convenience alias for the generated guest export namespace.
use exports::cherub::sandbox::tool as wit_tool;

// ─── StoreData ───────────────────────────────────────────────────────────────

/// Per-execution store data.
///
/// Created fresh for each WASM invocation. Holds the resource limiter,
/// host state, WASI context + resource table, and optional credential broker.
pub(crate) struct StoreData {
    limiter: WasmResourceLimiter,
    host_state: HostState,
    wasi: WasiCtx,
    table: ResourceTable,
    /// Dedicated single-threaded tokio runtime for HTTP I/O inside
    /// `spawn_blocking`. Lazily initialised on first `http_request` call.
    http_runtime: Option<tokio::runtime::Runtime>,
    /// Optional credential broker for host-side injection (requires `credentials` feature).
    #[cfg(feature = "credentials")]
    credential_broker: Option<Arc<crate::tools::credential_broker::CredentialBroker>>,
    /// User ID forwarded from the enforcement context (used by credential injection).
    #[cfg_attr(not(feature = "credentials"), allow(dead_code))]
    user_id: String,
}

impl StoreData {
    fn new(
        memory_limit: u64,
        host_state: HostState,
        user_id: String,
        #[cfg(feature = "credentials")] credential_broker: Option<
            Arc<crate::tools::credential_broker::CredentialBroker>,
        >,
    ) -> Self {
        // Minimal WASI context: no filesystem access, no env vars, no network.
        let wasi = WasiCtxBuilder::new().build();
        Self {
            limiter: WasmResourceLimiter::new(memory_limit),
            host_state,
            wasi,
            table: ResourceTable::new(),
            http_runtime: None,
            #[cfg(feature = "credentials")]
            credential_broker,
            user_id,
        }
    }
}

/// Implement `WasiView` for wasmtime-wasi v41.
///
/// `WasiView::ctx()` returns a `WasiCtxView<'_>` struct containing mutable
/// references to both the WASI context and the resource table.
impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// ─── Host trait implementation ────────────────────────────────────────────────

impl cherub::sandbox::host::Host for StoreData {
    fn log(&mut self, level: cherub::sandbox::host::LogLevel, message: String) {
        let lvl = match level {
            cherub::sandbox::host::LogLevel::Trace => LogLevel::Trace,
            cherub::sandbox::host::LogLevel::Debug => LogLevel::Debug,
            cherub::sandbox::host::LogLevel::Info => LogLevel::Info,
            cherub::sandbox::host::LogLevel::Warn => LogLevel::Warn,
            cherub::sandbox::host::LogLevel::Error => LogLevel::Error,
        };
        self.host_state.log(lvl, message);
    }

    fn now_millis(&mut self) -> u64 {
        self.host_state.now_millis()
    }

    fn workspace_read(&mut self, path: String) -> Option<String> {
        self.host_state.workspace_read(&path)
    }

    fn http_request(
        &mut self,
        method: String,
        url: String,
        headers_json: String,
        body: Option<Vec<u8>>,
        timeout_ms: Option<u32>,
    ) -> Result<cherub::sandbox::host::HttpResponse, String> {
        use std::collections::HashMap;

        // Parse and validate the URL.
        let parsed_url = Url::parse(&url).map_err(|e| format!("invalid URL: {e}"))?;
        let host = parsed_url
            .host_str()
            .ok_or_else(|| "URL has no host".to_owned())?
            .to_owned();

        // Allowlist and rate-limit check.
        self.host_state.check_http_request(&host)?;

        // DNS rebinding protection: resolve hostname and reject private IPs.
        reject_private_ip(&url)?;

        // Parse the caller-supplied headers (untrusted JSON from WASM).
        let extra_headers: HashMap<String, String> =
            serde_json::from_str(&headers_json).unwrap_or_default();

        // Validate caller-supplied header names to avoid header injection.
        for key in extra_headers.keys() {
            if key.contains('\n') || key.contains('\r') || key.contains(':') {
                return Err(format!("invalid header name: '{key}'"));
            }
        }

        // Start with caller-supplied headers. Credential injection may append more.
        let base_headers: Vec<(String, String)> = extra_headers.into_iter().collect();

        // Credential injection (requires `credentials` feature + configured broker).
        // We build a leak detector regardless so response bodies are always scanned.
        #[cfg(feature = "credentials")]
        let leak_detector = crate::tools::leak_detector::LeakDetector::new();

        #[cfg(feature = "credentials")]
        let final_headers: Vec<(String, String)> = {
            let mut h = base_headers;
            if let (Some(broker), Some(http_cap)) = (
                &self.credential_broker,
                self.host_state.capabilities().http.as_ref(),
            ) {
                for cred_name in &http_cap.credentials {
                    let rt = get_or_init_http_runtime(&mut self.http_runtime)?;
                    match rt.block_on(broker.inject(
                        &self.user_id,
                        cred_name,
                        &method,
                        parsed_url.clone(),
                    )) {
                        Ok(injection) => {
                            h.extend(injection.headers);
                            // Broker's leak_detector dropped; we use our own for the scan.
                            let _ = injection.leak_detector;
                        }
                        Err(e) => {
                            tracing::warn!(
                                credential = %cred_name,
                                error = %e,
                                "credential injection skipped"
                            );
                        }
                    }
                }
            }
            h
        };

        #[cfg(not(feature = "credentials"))]
        let final_headers = base_headers;

        // Timeout: caller-specified (default 30 s, capped at 300 s).
        let timeout_ms = timeout_ms.unwrap_or(30_000).min(300_000) as u64;
        let timeout = Duration::from_millis(timeout_ms);

        let method_upper = method.to_uppercase();
        let url_clone = url.clone();

        let rt = get_or_init_http_runtime(&mut self.http_runtime)?;
        let result = rt.block_on(async {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(30))
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("failed to build HTTP client: {e}"))?;

            let mut req = match method_upper.as_str() {
                "GET" => client.get(&url_clone),
                "POST" => client.post(&url_clone),
                "PUT" => client.put(&url_clone),
                "DELETE" => client.delete(&url_clone),
                "PATCH" => client.patch(&url_clone),
                "HEAD" => client.head(&url_clone),
                _ => return Err(format!("unsupported HTTP method: {method_upper}")),
            };
            for (k, v) in &final_headers {
                req = req.header(k, v);
            }
            if let Some(b) = body {
                req = req.body(b);
            }

            let response = req
                .send()
                .await
                .map_err(|e| format!("HTTP request failed: {e}"))?;

            let status = response.status().as_u16();
            let resp_headers: HashMap<String, String> = response
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|v| (k.as_str().to_string(), v.to_string()))
                })
                .collect();
            let headers_json_out =
                serde_json::to_string(&resp_headers).unwrap_or_else(|_| "{}".to_owned());

            // Cap response body size at 10 MiB to prevent memory exhaustion.
            const MAX_RESP: usize = 10 * 1024 * 1024;
            if let Some(cl) = response.content_length()
                && cl as usize > MAX_RESP
            {
                return Err(format!("response too large: {cl} bytes (max {MAX_RESP})"));
            }
            let body_bytes = response
                .bytes()
                .await
                .map_err(|e| format!("failed to read response body: {e}"))?;
            if body_bytes.len() > MAX_RESP {
                return Err(format!(
                    "response too large: {} bytes (max {MAX_RESP})",
                    body_bytes.len()
                ));
            }
            Ok((status, headers_json_out, body_bytes.to_vec()))
        });

        match result {
            Ok((status, headers_json_out, body)) => {
                // Scan response body for leaked credentials.
                let body_str = String::from_utf8_lossy(&body);
                #[cfg(feature = "credentials")]
                let body_str: std::borrow::Cow<'_, str> =
                    std::borrow::Cow::Owned(leak_detector.redact(&body_str));
                Ok(cherub::sandbox::host::HttpResponse {
                    status,
                    headers_json: headers_json_out,
                    body: body_str.into_owned().into_bytes(),
                })
            }
            Err(e) => Err(e),
        }
    }

    fn secret_exists(&mut self, name: String) -> bool {
        self.host_state.secret_exists(&name)
    }
}

/// Lazily initialise or retrieve the per-execution HTTP runtime.
fn get_or_init_http_runtime(
    rt: &mut Option<tokio::runtime::Runtime>,
) -> Result<&tokio::runtime::Runtime, String> {
    if rt.is_none() {
        *rt = Some(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("failed to create HTTP runtime: {e}"))?,
        );
    }
    Ok(rt.as_ref().expect("just initialized"))
}

// ─── WasmTool ────────────────────────────────────────────────────────────────

/// A tool backed by a WASM component.
///
/// Each call to [`execute`] creates a fresh `Store` and component instance
/// to ensure isolation between invocations.
pub struct WasmTool {
    pub(crate) module: Arc<PreparedModule>,
    runtime: Arc<WasmToolRuntime>,
    /// Optional credential broker (requires `credentials` feature).
    #[cfg(feature = "credentials")]
    broker: Option<Arc<crate::tools::credential_broker::CredentialBroker>>,
}

impl WasmTool {
    /// Create a new `WasmTool` from a prepared module.
    pub fn new(module: Arc<PreparedModule>, runtime: Arc<WasmToolRuntime>) -> Self {
        Self {
            module,
            runtime,
            #[cfg(feature = "credentials")]
            broker: None,
        }
    }

    /// Attach a credential broker for host-side injection.
    #[cfg(feature = "credentials")]
    pub fn with_broker(
        mut self,
        broker: Arc<crate::tools::credential_broker::CredentialBroker>,
    ) -> Self {
        self.broker = Some(broker);
        self
    }

    /// Tool name.
    pub fn name(&self) -> &str {
        &self.module.name
    }

    /// Execute this WASM tool.
    ///
    /// Requires a `CapabilityToken` (consumed on use). Runs in `spawn_blocking`.
    pub async fn execute(
        &self,
        params: &serde_json::Value,
        _token: CapabilityToken, // consumed — proves enforcement ran
        user_id: &str,
    ) -> Result<ToolResult, CherubError> {
        let module = Arc::clone(&self.module);
        let engine = self.runtime.engine.clone();
        let user_id = user_id.to_owned();
        let params = params.clone();
        #[cfg(feature = "credentials")]
        let broker = self.broker.clone();

        tokio::task::spawn_blocking(move || {
            execute_in_sandbox(
                &engine,
                &module,
                &params,
                &user_id,
                #[cfg(feature = "credentials")]
                broker,
            )
        })
        .await
        .map_err(|e| CherubError::Wasm(format!("WASM task panicked: {e}")))?
    }
}

/// Execute the WASM component synchronously inside `spawn_blocking`.
fn execute_in_sandbox(
    engine: &wasmtime::Engine,
    module: &PreparedModule,
    params: &serde_json::Value,
    user_id: &str,
    #[cfg(feature = "credentials")] broker: Option<
        Arc<crate::tools::credential_broker::CredentialBroker>,
    >,
) -> Result<ToolResult, CherubError> {
    let host_state = HostState::new(module.capabilities.clone()).with_user_id(user_id.to_owned());

    let store_data = StoreData::new(
        module.limits.memory_bytes,
        host_state,
        user_id.to_owned(),
        #[cfg(feature = "credentials")]
        broker,
    );

    let mut store = Store::new(engine, store_data);
    store.limiter(|data| &mut data.limiter);

    store
        .set_fuel(module.limits.fuel)
        .map_err(|e| CherubError::Wasm(format!("failed to set fuel: {e}")))?;

    let deadline = WasmToolRuntime::epoch_deadline(module.limits.timeout);
    store.set_epoch_deadline(deadline);
    store.epoch_deadline_trap();

    let mut linker: Linker<StoreData> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|e| CherubError::Wasm(format!("failed to add WASI to linker: {e}")))?;
    cherub::sandbox::host::add_to_linker::<_, HasSelf<_>>(&mut linker, |data| data)
        .map_err(|e| CherubError::Wasm(format!("failed to add host functions to linker: {e}")))?;

    // SAFETY: the bytes were produced by `Component::serialize()` with the same
    // engine config as this store. BLAKE3 hash was verified at load time in
    // loader.rs, ensuring the bytes haven't been tampered with.
    let component = unsafe { Component::deserialize(engine, module.component_bytes()) }
        .map_err(|e| CherubError::Wasm(format!("component deserialize failed: {e}")))?;

    let tool_instance = SandboxedTool::instantiate(&mut store, &component, &linker)
        .map_err(|e| CherubError::Wasm(format!("instantiation failed: {e}")))?;

    let params_json = serde_json::to_string(params)
        .map_err(|e| CherubError::Wasm(format!("failed to serialize params: {e}")))?;

    let req = wit_tool::Request {
        params: params_json,
        context: None,
    };

    let response = tool_instance
        .cherub_sandbox_tool()
        .call_execute(&mut store, &req)
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("fuel") || msg.contains("out of fuel") {
                CherubError::Wasm(format!(
                    "WASM tool '{}' exceeded its fuel budget",
                    module.name
                ))
            } else if msg.contains("epoch") || msg.contains("interrupt") {
                CherubError::Wasm(format!(
                    "WASM tool '{}' exceeded its time limit",
                    module.name
                ))
            } else {
                CherubError::Wasm(format!("WASM execution failed: {e}"))
            }
        })?;

    store.data().host_state.emit_logs(&module.name);

    if let Some(err) = response.error {
        return Err(CherubError::ToolExecution(format!(
            "WASM tool '{}' returned error: {err}",
            module.name
        )));
    }
    Ok(ToolResult {
        output: response.output.unwrap_or_default(),
    })
}

// ─── Metadata extraction ─────────────────────────────────────────────────────

/// Extract `description` and `schema` from a compiled component.
///
/// Creates a minimal execution environment, calls `description()` and
/// `schema()` on the guest, then discards the store. Called once at load time.
pub(crate) fn extract_metadata(
    engine: &wasmtime::Engine,
    component_bytes: &[u8],
    capabilities: &crate::tools::wasm::capabilities::Capabilities,
) -> Result<(String, serde_json::Value), CherubError> {
    let host_state = HostState::new(capabilities.clone());
    let store_data = StoreData::new(
        10 * 1024 * 1024,
        host_state,
        String::new(),
        #[cfg(feature = "credentials")]
        None,
    );

    let mut store = Store::new(engine, store_data);
    store.limiter(|data| &mut data.limiter);
    store
        .set_fuel(1_000_000)
        .map_err(|e| CherubError::Wasm(format!("failed to set fuel for metadata: {e}")))?;

    let mut linker: Linker<StoreData> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|e| CherubError::Wasm(format!("WASI linker failed: {e}")))?;
    cherub::sandbox::host::add_to_linker::<_, HasSelf<_>>(&mut linker, |data| data)
        .map_err(|e| CherubError::Wasm(format!("host linker failed: {e}")))?;

    // SAFETY: same engine config invariant as execute_in_sandbox.
    let component = unsafe { Component::deserialize(engine, component_bytes) }.map_err(|e| {
        CherubError::Wasm(format!("component deserialize for metadata failed: {e}"))
    })?;

    let instance = SandboxedTool::instantiate(&mut store, &component, &linker)
        .map_err(|e| CherubError::Wasm(format!("metadata instantiation failed: {e}")))?;

    let description = instance
        .cherub_sandbox_tool()
        .call_description(&mut store)
        .map_err(|e| CherubError::Wasm(format!("description() call failed: {e}")))?;

    let schema_json = instance
        .cherub_sandbox_tool()
        .call_schema(&mut store)
        .map_err(|e| CherubError::Wasm(format!("schema() call failed: {e}")))?;

    let schema: serde_json::Value = serde_json::from_str(&schema_json).map_err(|e| {
        CherubError::Wasm(format!(
            "component returned invalid JSON schema: {e} (raw: {schema_json:?})"
        ))
    })?;

    Ok((description, schema))
}
