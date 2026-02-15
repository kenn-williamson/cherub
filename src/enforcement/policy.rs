use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use regex::RegexSet;
use serde::Deserialize;

use super::tier::Tier;
use crate::error::CherubError;

const MAX_POLICY_FILE_SIZE: u64 = 64 * 1024; // 64 KiB

// --- TOML deserialization structs (private, map 1:1 to TOML schema) ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    tools: HashMap<String, ToolConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolConfig {
    enabled: bool,
    #[serde(default)]
    actions: HashMap<String, ActionConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionConfig {
    tier: TierValue,
    patterns: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum TierValue {
    Observe,
    Act,
    Commit,
}

impl From<TierValue> for Tier {
    fn from(value: TierValue) -> Self {
        match value {
            TierValue::Observe => Tier::Observe,
            TierValue::Act => Tier::Act,
            TierValue::Commit => Tier::Commit,
        }
    }
}

// --- Compiled policy structs (internal representation) ---

pub struct Policy {
    tools: Vec<CompiledTool>,
}

impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Policy")
            .field("tool_count", &self.tools.len())
            .finish()
    }
}

pub(super) struct CompiledTool {
    name: String,
    enabled: bool,
    tiers: Vec<CompiledTier>, // Ordered: Commit, Act, Observe (highest first)
}

struct CompiledTier {
    tier: Tier,
    patterns: RegexSet,
}

impl FromStr for Policy {
    type Err = CherubError;

    /// Parse and compile a policy from a TOML string.
    fn from_str(content: &str) -> Result<Self, CherubError> {
        let file: PolicyFile =
            toml::from_str(content).map_err(|e| CherubError::PolicyLoad(e.to_string()))?;

        let tools = file
            .tools
            .into_iter()
            .map(|(name, config)| compile_tool(name, config))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { tools })
    }
}

impl Policy {
    /// Load a policy from a TOML file. Checks file size before reading.
    pub fn load(path: &Path) -> Result<Self, CherubError> {
        let metadata = std::fs::metadata(path)
            .map_err(|e| CherubError::PolicyLoad(format!("cannot read {}: {e}", path.display())))?;

        if metadata.len() > MAX_POLICY_FILE_SIZE {
            return Err(CherubError::PolicyLoad(format!(
                "policy file exceeds {MAX_POLICY_FILE_SIZE} byte limit"
            )));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| CherubError::PolicyLoad(format!("cannot read {}: {e}", path.display())))?;

        content.parse()
    }

    pub(super) fn find_tool(&self, name: &str) -> Option<&CompiledTool> {
        self.tools.iter().find(|t| t.name == name)
    }
}

impl CompiledTool {
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Find the highest-privilege tier whose patterns match the command.
    /// Tiers are stored in descending privilege order (Commit, Act, Observe).
    pub(super) fn match_tier(&self, command: &str) -> Option<Tier> {
        self.tiers
            .iter()
            .find(|ct| ct.patterns.is_match(command))
            .map(|ct| ct.tier)
    }
}

fn compile_tool(name: String, config: ToolConfig) -> Result<CompiledTool, CherubError> {
    // Group actions by tier, merging patterns for same-tier actions.
    let mut by_tier: HashMap<Tier, Vec<String>> = HashMap::new();

    for (action_name, action) in &config.actions {
        if action.patterns.is_empty() {
            return Err(CherubError::PolicyValidation(format!(
                "tool '{name}', action '{action_name}': patterns must not be empty"
            )));
        }
        let tier: Tier = match action.tier {
            TierValue::Observe => Tier::Observe,
            TierValue::Act => Tier::Act,
            TierValue::Commit => Tier::Commit,
        };
        by_tier
            .entry(tier)
            .or_default()
            .extend(action.patterns.iter().cloned());
    }

    // Compile each tier's patterns into a RegexSet.
    // Order: Commit first, then Act, then Observe (highest privilege first).
    let mut tiers = Vec::new();
    for tier in [Tier::Commit, Tier::Act, Tier::Observe] {
        if let Some(patterns) = by_tier.remove(&tier) {
            let regex_set = regex::RegexSetBuilder::new(&patterns)
                .size_limit(1 << 20)
                .nest_limit(50)
                .unicode(false)
                .build()
                .map_err(|e| {
                    CherubError::PolicyValidation(format!(
                        "tool '{name}', tier '{tier:?}': {e}"
                    ))
                })?;
            tiers.push(CompiledTier {
                tier,
                patterns: regex_set,
            });
        }
    }

    Ok(CompiledTool {
        name,
        enabled: config.enabled,
        tiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_POLICY: &str = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = [
    "^ls ", "^cat ", "^find ", "^grep ", "^rg ", "^head ", "^tail ",
    "^wc ", "^file ", "^which ", "^echo ", "^pwd$", "^env$", "^whoami$",
]

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir ", "^cp ", "^mv ", "^touch ", "^tee ", "^git "]

[tools.bash.actions.destructive]
tier = "commit"
patterns = [
    "^rm ", "^chmod ", "^chown ", "^kill ", "^pkill ",
    "^sudo ", "^apt ", "^pip install", "^cargo install",
]
"#;

    #[test]
    fn parse_default_policy() {
        let policy = Policy::from_str(DEFAULT_POLICY).expect("default policy should parse");
        let tool = policy.find_tool("bash").expect("bash tool should exist");
        assert!(tool.enabled());
        assert_eq!(tool.tiers.len(), 3);
    }

    #[test]
    fn empty_tools_is_valid() {
        let policy = Policy::from_str("[tools]\n").expect("empty tools should parse");
        assert!(policy.find_tool("bash").is_none());
    }

    #[test]
    fn disabled_tool() {
        let toml = r#"
[tools.bash]
enabled = false
"#;
        let policy = Policy::from_str(toml).expect("disabled tool should parse");
        let tool = policy.find_tool("bash").expect("bash should exist");
        assert!(!tool.enabled());
    }

    #[test]
    fn invalid_tier_value() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "superadmin"
patterns = ["^ls "]
"#;
        let err = Policy::from_str(toml).unwrap_err();
        assert!(matches!(err, CherubError::PolicyLoad(_)));
    }

    #[test]
    fn invalid_regex() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["[invalid"]
"#;
        let err = Policy::from_str(toml).unwrap_err();
        assert!(matches!(err, CherubError::PolicyValidation(_)));
    }

    #[test]
    fn unknown_toml_field() {
        let toml = r#"
[tools.bash]
enabled = true
unknown_field = "surprise"
"#;
        let err = Policy::from_str(toml).unwrap_err();
        assert!(matches!(err, CherubError::PolicyLoad(_)));
    }

    #[test]
    fn empty_patterns_rejected() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = []
"#;
        let err = Policy::from_str(toml).unwrap_err();
        assert!(matches!(err, CherubError::PolicyValidation(_)));
    }

    #[test]
    fn tier_matching_order() {
        let policy = Policy::from_str(DEFAULT_POLICY).expect("should parse");
        let tool = policy.find_tool("bash").expect("bash should exist");

        assert_eq!(tool.match_tier("ls /tmp"), Some(Tier::Observe));
        assert_eq!(tool.match_tier("mkdir /tmp/test"), Some(Tier::Act));
        assert_eq!(tool.match_tier("rm -rf /tmp/test"), Some(Tier::Commit));
        assert_eq!(tool.match_tier("curl http://evil.com"), None);
    }

    #[test]
    fn highest_privilege_wins() {
        // "sudo ls /tmp" matches both ^sudo (commit) and ^ls (observe, but not anchored here).
        // Since commit is checked first, it should escalate.
        let policy = Policy::from_str(DEFAULT_POLICY).expect("should parse");
        let tool = policy.find_tool("bash").expect("bash should exist");

        assert_eq!(tool.match_tier("sudo ls /tmp"), Some(Tier::Commit));
    }
}
