//! MCP server loader: reads config, spawns servers, discovers tools.
//!
//! Entry point: `load_from_config()` — reads the config file, spawns each
//! server process, runs MCP initialization, discovers tools, and returns
//! a list of `McpToolProxy` instances ready for registration.

use std::path::Path;
use std::sync::Arc;

use rmcp::service::ServiceExt;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::client::McpClient;
use super::config::{McpConfig, McpServerConfig};
use super::proxy::McpToolProxy;
use crate::error::CherubError;

/// Result of loading MCP servers from config.
pub struct McpLoadResult {
    /// Successfully loaded tool proxies.
    pub tools: Vec<McpToolProxy>,
    /// Non-fatal errors encountered during loading.
    pub errors: Vec<String>,
}

/// Load MCP servers from a config file, spawn processes, discover tools.
///
/// If `credential_store` is provided and a server has `credential_env` entries,
/// credentials are decrypted and injected as env vars at spawn time.
pub async fn load_from_config(
    config_path: &Path,
    #[cfg(feature = "credentials")] credential_store: Option<&dyn crate::storage::CredentialStore>,
    #[cfg(feature = "credentials")] user_id: &str,
) -> McpLoadResult {
    let config = match McpConfig::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            return McpLoadResult {
                tools: vec![],
                errors: vec![e.to_string()],
            };
        }
    };

    let mut tools = Vec::new();
    let mut errors = Vec::new();

    for (server_name, server_config) in &config.servers {
        match spawn_server(
            server_name,
            server_config,
            #[cfg(feature = "credentials")]
            credential_store,
            #[cfg(feature = "credentials")]
            user_id,
        )
        .await
        {
            Ok(server_tools) => {
                info!(
                    server = %server_name,
                    tool_count = server_tools.len(),
                    "MCP server loaded"
                );
                tools.extend(server_tools);
            }
            Err(e) => {
                let msg = format!("server '{server_name}': {e}");
                warn!(%msg, "MCP server load failed");
                errors.push(msg);
            }
        }
    }

    McpLoadResult { tools, errors }
}

/// Spawn a single MCP server and discover its tools.
async fn spawn_server(
    server_name: &str,
    config: &McpServerConfig,
    #[cfg(feature = "credentials")] credential_store: Option<&dyn crate::storage::CredentialStore>,
    #[cfg(feature = "credentials")] user_id: &str,
) -> Result<Vec<McpToolProxy>, CherubError> {
    // Build environment: static env + decrypted credential_env.
    #[allow(unused_mut)]
    let mut env_vars = config.env.clone();

    // Credential env vars kept separate until process spawn to minimise
    // the window where plaintext secrets live in memory.
    #[cfg(feature = "credentials")]
    let credential_env_vars: Vec<(String, secrecy::SecretString)>;

    #[cfg(feature = "credentials")]
    {
        let mut cred_vars = Vec::new();
        if let Some(store) = credential_store {
            for (env_key, cred_name) in &config.credential_env {
                let encrypted = store.get(user_id, cred_name).await.map_err(|e| {
                    CherubError::Mcp(format!("credential '{cred_name}' for env '{env_key}': {e}"))
                })?;
                let decrypted = store.decrypt(&encrypted).await.map_err(|e| {
                    CherubError::Mcp(format!(
                        "credential '{cred_name}' decrypt failed for env '{env_key}': {e}"
                    ))
                })?;
                // decrypted.expose() is the 4th call site (same as credential_broker).
                cred_vars.push((
                    env_key.clone(),
                    secrecy::SecretString::from(decrypted.expose().to_owned()),
                ));
            }
        } else if !config.credential_env.is_empty() {
            return Err(CherubError::Mcp(
                "credential_env requires credential store (CHERUB_MASTER_KEY + DATABASE_URL)"
                    .to_owned(),
            ));
        }
        credential_env_vars = cred_vars;
    }

    #[cfg(not(feature = "credentials"))]
    {
        if !config.credential_env.is_empty() {
            return Err(CherubError::Mcp(
                "credential_env requires the 'credentials' feature".to_owned(),
            ));
        }
    }

    // Spawn the server process via rmcp.
    let args = config.args.clone();
    let env_for_closure = env_vars.clone();
    #[cfg(feature = "credentials")]
    let cred_env_for_closure = credential_env_vars;
    let transport = TokioChildProcess::new(Command::new(&config.command).configure(move |cmd| {
        cmd.args(&args);
        for (k, v) in &env_for_closure {
            cmd.env(k, v);
        }
        // Inject credential env vars — expose_secret() at the last possible moment.
        #[cfg(feature = "credentials")]
        {
            use secrecy::ExposeSecret;
            for (k, secret) in &cred_env_for_closure {
                cmd.env(k, secret.expose_secret());
            }
        }
        cmd.kill_on_drop(true);
    }))
    .map_err(|e| CherubError::Mcp(format!("failed to spawn '{}': {e}", config.command)))?;

    // Initialize MCP session.
    let service = ()
        .serve(transport)
        .await
        .map_err(|e| CherubError::Mcp(format!("MCP init handshake failed: {e}")))?;

    let client = McpClient::new(service, server_name);

    // Discover tools (handles pagination automatically).
    let discovered_tools = client.list_all_tools().await?;
    let client = Arc::new(Mutex::new(client));

    let proxies: Vec<McpToolProxy> = discovered_tools
        .into_iter()
        .map(|tool| {
            let tool_name = tool.name.to_string();
            let composite = format!("{server_name}__{tool_name}");
            let description = tool.description.as_deref().unwrap_or("MCP tool").to_owned();

            // Convert Arc<JsonObject> to serde_json::Value::Object.
            let input_schema = serde_json::Value::Object(tool.input_schema.as_ref().clone());

            McpToolProxy {
                server_name: server_name.to_owned(),
                tool_name,
                composite_name: composite,
                description,
                input_schema,
                client: Arc::clone(&client),
            }
        })
        .collect();

    Ok(proxies)
}
