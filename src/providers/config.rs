//! Provider and sub-agent configuration.
//!
//! Parsed from a TOML file (`--providers`). Each provider entry specifies
//! the type, model, optional base URL, and which env var holds the API key.
//! Sub-agent entries reference a provider by name and add a system prompt
//! (wired in M13d).

use std::collections::HashMap;
use std::path::Path;

use secrecy::SecretString;
use serde::Deserialize;

use super::Provider;
use super::anthropic::AnthropicProvider;
use super::openai::OpenAiProvider;
use crate::error::CherubError;

const MAX_CONFIG_FILE_SIZE: u64 = 64 * 1024; // 64 KiB

/// Top-level providers configuration file.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    /// Named provider definitions. The key `"default"` is used as the main
    /// orchestrator provider when `--providers` is specified.
    pub providers: HashMap<String, ProviderDef>,

    /// Named sub-agent definitions. Each becomes a tool in the orchestrator's
    /// ToolRegistry (M13d). Parsed here but not wired until M13d.
    #[serde(default)]
    pub agents: HashMap<String, SubAgentDef>,
}

/// Which provider backend to use.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    Anthropic,
    Openai,
    /// Wraps multiple providers for automatic failover (M13c).
    Failover,
}

/// Configuration for a single provider.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDef {
    /// Provider backend type.
    #[serde(rename = "type")]
    pub provider_type: ProviderType,

    /// Model identifier (e.g., "claude-sonnet-4-20250514", "gpt-4o").
    pub model: String,

    /// Name of environment variable holding the API key.
    /// Defaults to `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` based on type.
    /// Optional for local providers (Ollama, vLLM, etc.).
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Custom base URL for OpenAI-compatible endpoints.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Maximum output tokens per completion call.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// For failover providers (M13c): ordered list of provider names to try.
    #[serde(default)]
    pub providers: Option<Vec<String>>,
}

fn default_max_tokens() -> u32 {
    4096
}

/// Configuration for a sub-agent tool (M13d).
///
/// Each sub-agent becomes a tool that the orchestrator can invoke.
/// The orchestrator sees the `description` and decides when to delegate.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentDef {
    /// Human-readable description shown to the orchestrator model.
    pub description: String,

    /// Name of the provider to use (must exist in `[providers]`).
    pub provider: String,

    /// System prompt defining this sub-agent's role and behavior.
    pub system_prompt: String,

    /// Maximum turns the sub-agent can execute (default: 1).
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,

    /// Timeout in seconds for the entire sub-agent execution.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    /// Tool names the sub-agent has access to. Empty = no tools (pure completion).
    #[serde(default)]
    pub tools: Vec<String>,
}

fn default_max_turns() -> u32 {
    1
}

fn default_timeout_secs() -> u64 {
    120
}

impl ProvidersConfig {
    /// Load and parse providers config from a TOML file.
    pub fn load(path: &Path) -> Result<Self, CherubError> {
        let metadata = std::fs::metadata(path)
            .map_err(|e| CherubError::Config(format!("cannot read {}: {e}", path.display())))?;

        if metadata.len() > MAX_CONFIG_FILE_SIZE {
            return Err(CherubError::Config(format!(
                "config file exceeds {MAX_CONFIG_FILE_SIZE} byte limit"
            )));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| CherubError::Config(format!("cannot read {}: {e}", path.display())))?;

        let config: Self = toml::from_str(&content)
            .map_err(|e| CherubError::Config(format!("invalid providers config: {e}")))?;

        config.validate()?;
        Ok(config)
    }

    /// Validate cross-field constraints that serde can't enforce.
    fn validate(&self) -> Result<(), CherubError> {
        for (name, def) in &self.providers {
            // Failover providers must declare a provider list.
            if def.provider_type == ProviderType::Failover && def.providers.is_none() {
                return Err(CherubError::Config(format!(
                    "provider '{name}': failover type requires a 'providers' list"
                )));
            }
            // Non-failover providers must not declare a provider list.
            if def.provider_type != ProviderType::Failover && def.providers.is_some() {
                return Err(CherubError::Config(format!(
                    "provider '{name}': 'providers' list is only valid for failover type"
                )));
            }
        }

        // Validate sub-agent provider references.
        for (name, agent) in &self.agents {
            if !self.providers.contains_key(&agent.provider) {
                return Err(CherubError::Config(format!(
                    "agent '{name}': references unknown provider '{}'",
                    agent.provider
                )));
            }
        }

        Ok(())
    }
}

/// Instantiate a concrete provider from a definition.
///
/// Reads the API key from the environment variable named in `api_key_env`.
/// Reusable by `main.rs`, `telegram.rs`, and sub-agent setup (M13d).
pub fn instantiate_provider(def: &ProviderDef) -> Result<Box<dyn Provider>, CherubError> {
    match def.provider_type {
        ProviderType::Anthropic => {
            let key_env = def.api_key_env.as_deref().unwrap_or("ANTHROPIC_API_KEY");
            let key_raw = std::env::var(key_env)
                .map_err(|_| CherubError::Config(format!("{key_env} not set")))?;
            if key_raw.is_empty() {
                return Err(CherubError::Config(format!("{key_env} is empty")));
            }
            let provider =
                AnthropicProvider::new(SecretString::from(key_raw), &def.model, def.max_tokens)?;
            Ok(Box::new(provider))
        }
        ProviderType::Openai => {
            let api_key = def
                .api_key_env
                .as_deref()
                .and_then(|env| std::env::var(env).ok())
                .filter(|k| !k.is_empty())
                .map(SecretString::from);
            let mut provider = OpenAiProvider::new(api_key, &def.model, def.max_tokens)?;
            if let Some(ref url) = def.base_url {
                provider = provider.with_base_url(url.clone());
            }
            Ok(Box::new(provider))
        }
        ProviderType::Failover => Err(CherubError::Config(
            "failover provider type is not yet implemented (M13c)".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_providers() {
        let toml = r#"
[providers.default]
type = "anthropic"
model = "claude-sonnet-4-20250514"
api_key_env = "ANTHROPIC_API_KEY"

[providers.gpt4o]
type = "openai"
model = "gpt-4o"
api_key_env = "OPENAI_API_KEY"
max_tokens = 2048
"#;
        let config: ProvidersConfig = toml::from_str(toml).expect("should parse");
        assert_eq!(config.providers.len(), 2);

        let default = &config.providers["default"];
        assert_eq!(default.provider_type, ProviderType::Anthropic);
        assert_eq!(default.model, "claude-sonnet-4-20250514");
        assert_eq!(default.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(default.max_tokens, 4096); // default

        let gpt = &config.providers["gpt4o"];
        assert_eq!(gpt.provider_type, ProviderType::Openai);
        assert_eq!(gpt.max_tokens, 2048);
    }

    #[test]
    fn parse_openai_with_base_url() {
        let toml = r#"
[providers.local]
type = "openai"
model = "llama3"
base_url = "http://localhost:11434/v1"
"#;
        let config: ProvidersConfig = toml::from_str(toml).expect("should parse");
        let local = &config.providers["local"];
        assert_eq!(local.base_url.as_deref(), Some("http://localhost:11434/v1"));
        assert!(local.api_key_env.is_none());
    }

    #[test]
    fn parse_with_agents() {
        let toml = r#"
[providers.local]
type = "openai"
model = "llama3"
base_url = "http://localhost:11434/v1"

[agents.summarizer]
description = "Summarize documents"
provider = "local"
system_prompt = "You are a summarizer."
max_turns = 1
timeout_secs = 60
tools = ["bash", "file"]
"#;
        let config: ProvidersConfig = toml::from_str(toml).expect("should parse");
        assert_eq!(config.agents.len(), 1);

        let agent = &config.agents["summarizer"];
        assert_eq!(agent.description, "Summarize documents");
        assert_eq!(agent.provider, "local");
        assert_eq!(agent.max_turns, 1);
        assert_eq!(agent.timeout_secs, 60);
        assert_eq!(agent.tools, vec!["bash", "file"]);
    }

    #[test]
    fn agent_defaults() {
        let toml = r#"
[providers.local]
type = "openai"
model = "llama3"
base_url = "http://localhost:11434/v1"

[agents.simple]
description = "Simple agent"
provider = "local"
system_prompt = "You are helpful."
"#;
        let config: ProvidersConfig = toml::from_str(toml).expect("should parse");
        let agent = &config.agents["simple"];
        assert_eq!(agent.max_turns, 1);
        assert_eq!(agent.timeout_secs, 120);
        assert!(agent.tools.is_empty());
    }

    #[test]
    fn unknown_field_rejected() {
        let toml = r#"
[providers.test]
type = "anthropic"
model = "claude-sonnet-4-20250514"
unknown_field = "bad"
"#;
        assert!(toml::from_str::<ProvidersConfig>(toml).is_err());
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        let toml = r#"
bad_section = true

[providers.test]
type = "anthropic"
model = "claude-sonnet-4-20250514"
"#;
        assert!(toml::from_str::<ProvidersConfig>(toml).is_err());
    }

    #[test]
    fn unknown_agent_field_rejected() {
        let toml = r#"
[providers.local]
type = "openai"
model = "llama3"

[agents.bad]
description = "test"
provider = "local"
system_prompt = "test"
unknown = true
"#;
        assert!(toml::from_str::<ProvidersConfig>(toml).is_err());
    }

    #[test]
    fn empty_providers_valid() {
        let toml = "[providers]\n";
        let config: ProvidersConfig = toml::from_str(toml).expect("should parse");
        assert!(config.providers.is_empty());
        assert!(config.agents.is_empty());
    }

    #[test]
    fn validate_failover_requires_providers_list() {
        let toml = r#"
[providers.bad-failover]
type = "failover"
model = "failover"
"#;
        let config: ProvidersConfig = toml::from_str(toml).expect("should parse");
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("requires a 'providers' list"));
    }

    #[test]
    fn validate_non_failover_rejects_providers_list() {
        let toml = r#"
[providers.bad]
type = "anthropic"
model = "claude-sonnet-4-20250514"
providers = ["other"]
"#;
        let config: ProvidersConfig = toml::from_str(toml).expect("should parse");
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("only valid for failover"));
    }

    #[test]
    fn validate_agent_references_unknown_provider() {
        let toml = r#"
[providers.local]
type = "openai"
model = "llama3"

[agents.bad]
description = "test"
provider = "nonexistent"
system_prompt = "test"
"#;
        let config: ProvidersConfig = toml::from_str(toml).expect("should parse");
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("unknown provider 'nonexistent'"));
    }

    #[test]
    fn instantiate_openai_no_api_key() {
        // Local providers (Ollama, etc.) don't need an API key.
        let def = ProviderDef {
            provider_type: ProviderType::Openai,
            model: "llama3".to_owned(),
            api_key_env: None,
            base_url: Some("http://localhost:11434/v1".to_owned()),
            max_tokens: 2048,
            providers: None,
        };
        let provider = instantiate_provider(&def).expect("should succeed without API key");
        assert_eq!(provider.model_name(), "llama3");
        assert_eq!(provider.max_output_tokens(), 2048);
    }

    #[test]
    fn instantiate_anthropic_missing_env_var() {
        let def = ProviderDef {
            provider_type: ProviderType::Anthropic,
            model: "claude-sonnet-4-20250514".to_owned(),
            api_key_env: Some("CHERUB_TEST_NONEXISTENT_KEY_12345".to_owned()),
            base_url: None,
            max_tokens: 4096,
            providers: None,
        };
        match instantiate_provider(&def) {
            Err(e) => assert!(
                e.to_string().contains("CHERUB_TEST_NONEXISTENT_KEY_12345"),
                "unexpected error: {e}"
            ),
            Ok(_) => panic!("expected error for missing env var"),
        }
    }

    #[test]
    fn instantiate_failover_returns_error() {
        let def = ProviderDef {
            provider_type: ProviderType::Failover,
            model: "failover".to_owned(),
            api_key_env: None,
            base_url: None,
            max_tokens: 4096,
            providers: Some(vec!["a".to_owned(), "b".to_owned()]),
        };
        match instantiate_provider(&def) {
            Err(e) => assert!(
                e.to_string().contains("not yet implemented"),
                "unexpected error: {e}"
            ),
            Ok(_) => panic!("expected error for failover"),
        }
    }
}
