//! MCP server configuration.
//!
//! Parsed from a TOML file (`--mcp-config`). Each server entry specifies the
//! command to spawn, arguments, environment variables, and optional credential
//! references for env-var injection at spawn time.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::CherubError;

const MAX_CONFIG_FILE_SIZE: u64 = 64 * 1024; // 64 KiB

/// Top-level MCP configuration file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    pub servers: HashMap<String, McpServerConfig>,
}

/// Configuration for a single MCP server process.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Command to spawn (e.g., "npx", "uvx", "node").
    pub command: String,
    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Static environment variables injected at spawn time.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Credential references: env var name → credential name in the vault.
    /// Decrypted at spawn time and injected as env vars.
    #[serde(default)]
    pub credential_env: HashMap<String, String>,
}

impl McpConfig {
    /// Load and parse MCP config from a TOML file.
    pub fn load(path: &Path) -> Result<Self, CherubError> {
        let metadata = std::fs::metadata(path)
            .map_err(|e| CherubError::Mcp(format!("cannot read {}: {e}", path.display())))?;

        if metadata.len() > MAX_CONFIG_FILE_SIZE {
            return Err(CherubError::Mcp(format!(
                "config file exceeds {MAX_CONFIG_FILE_SIZE} byte limit"
            )));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| CherubError::Mcp(format!("cannot read {}: {e}", path.display())))?;

        toml::from_str(&content).map_err(|e| CherubError::Mcp(format!("invalid MCP config: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_config() {
        let toml = r#"
[servers.google-workspace]
command = "npx"
args = ["-y", "@anthropic/mcp-server-google-workspace"]
env = { GOOGLE_APPLICATION_CREDENTIALS = "/path/to/creds.json" }

[servers.fireflies]
command = "uvx"
args = ["mcp-server-fireflies"]
"#;
        let config: McpConfig = toml::from_str(toml).expect("basic config should parse");
        assert_eq!(config.servers.len(), 2);

        let gw = &config.servers["google-workspace"];
        assert_eq!(gw.command, "npx");
        assert_eq!(
            gw.args,
            vec!["-y", "@anthropic/mcp-server-google-workspace"]
        );
        assert_eq!(
            gw.env.get("GOOGLE_APPLICATION_CREDENTIALS").unwrap(),
            "/path/to/creds.json"
        );
        assert!(gw.credential_env.is_empty());

        let ff = &config.servers["fireflies"];
        assert_eq!(ff.command, "uvx");
        assert!(ff.env.is_empty());
    }

    #[test]
    fn parse_config_with_credential_env() {
        let toml = r#"
[servers.stripe]
command = "node"
args = ["mcp-server-stripe.js"]
credential_env = { STRIPE_API_KEY = "stripe_key" }
"#;
        let config: McpConfig = toml::from_str(toml).expect("credential_env config should parse");
        let stripe = &config.servers["stripe"];
        assert_eq!(
            stripe.credential_env.get("STRIPE_API_KEY").unwrap(),
            "stripe_key"
        );
    }

    #[test]
    fn unknown_field_rejected() {
        let toml = r#"
[servers.test]
command = "echo"
unknown = "bad"
"#;
        let err = toml::from_str::<McpConfig>(toml);
        assert!(err.is_err());
    }

    #[test]
    fn empty_servers_valid() {
        let toml = "[servers]\n";
        let config: McpConfig = toml::from_str(toml).expect("empty servers should parse");
        assert!(config.servers.is_empty());
    }
}
