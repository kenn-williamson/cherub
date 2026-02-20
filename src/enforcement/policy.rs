use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use regex::{Regex, RegexSet};
use serde::Deserialize;
use tracing::{info, info_span};

use super::extraction::MatchSource;
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
#[serde(rename_all = "snake_case")]
enum MatchSourceValue {
    Command,
    Structured,
    /// For the `http` tool: extracts `"{method}:{host}"` from params.
    HttpStructured,
}

impl From<MatchSourceValue> for MatchSource {
    fn from(v: MatchSourceValue) -> Self {
        match v {
            MatchSourceValue::Command => MatchSource::Command,
            MatchSourceValue::Structured => MatchSource::Structured,
            MatchSourceValue::HttpStructured => MatchSource::HttpStructured,
        }
    }
}

fn default_match_source() -> MatchSourceValue {
    MatchSourceValue::Command
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolConfig {
    enabled: bool,
    #[serde(default = "default_match_source")]
    match_source: MatchSourceValue,
    #[serde(default)]
    actions: HashMap<String, ActionConfig>,
    #[serde(default)]
    constraints: Vec<ConstraintConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionConfig {
    tier: TierValue,
    patterns: Vec<String>,
    #[serde(default)]
    constraints: Vec<ConstraintConfig>,
    #[serde(default)]
    on_constraint_failure: Option<OnConstraintFailureValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConstraintConfig {
    field: String,
    op: ConstraintOp,
    value: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConstraintOp {
    Eq,
    Lt,
    Gt,
    Contains,
    NotContains,
    OneOf,
    ContainsAll,
    Matches,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum OnConstraintFailureValue {
    Reject,
    Escalate,
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

#[derive(Clone)]
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

#[derive(Clone)]
pub(super) struct CompiledTool {
    name: String,
    enabled: bool,
    match_source: MatchSource, // How to extract action strings from params
    actions: Vec<CompiledAction>, // Ordered: Commit first, then Act, then Observe
    constraints: Vec<CompiledConstraint>, // Tool-level: hard reject on failure
}

#[derive(Clone)]
pub(super) struct CompiledAction {
    #[allow(dead_code)] // Used for diagnostics and future constraint error messages
    pub(super) name: String,
    pub(super) tier: Tier,
    patterns: RegexSet,
    pub(super) constraints: Vec<CompiledConstraint>,
    pub(super) on_constraint_failure: OnConstraintFailure,
}

#[derive(Clone)]
pub(super) struct CompiledConstraint {
    field: String,
    predicate: Predicate,
}

#[derive(Clone)]
enum Predicate {
    Eq(serde_json::Value),
    Lt(f64),
    Gt(f64),
    Contains(String),
    NotContains(String),
    OneOf(Vec<serde_json::Value>),
    ContainsAll(Vec<String>),
    Matches(Regex),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum OnConstraintFailure {
    Reject,
    Escalate,
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
        let _span = info_span!("policy_load", path = %path.display()).entered();

        let metadata = std::fs::metadata(path)
            .map_err(|e| CherubError::PolicyLoad(format!("cannot read {}: {e}", path.display())))?;

        if metadata.len() > MAX_POLICY_FILE_SIZE {
            return Err(CherubError::PolicyLoad(format!(
                "policy file exceeds {MAX_POLICY_FILE_SIZE} byte limit"
            )));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| CherubError::PolicyLoad(format!("cannot read {}: {e}", path.display())))?;

        let policy: Self = content.parse()?;
        info!(tool_count = policy.tools.len(), "policy compiled");
        Ok(policy)
    }

    pub(super) fn find_tool(&self, name: &str) -> Option<&CompiledTool> {
        self.tools.iter().find(|t| t.name == name)
    }
}

impl CompiledConstraint {
    /// Evaluate this constraint against the params JSON.
    /// Missing field → false (deny by default).
    fn evaluate(&self, params: &serde_json::Value) -> bool {
        match params.get(&self.field) {
            None => false,
            Some(value) => self.predicate.evaluate(value),
        }
    }
}

impl Predicate {
    fn evaluate(&self, value: &serde_json::Value) -> bool {
        match self {
            Predicate::Eq(expected) => value == expected,
            Predicate::Lt(bound) => value.as_f64().is_some_and(|v| v < *bound),
            Predicate::Gt(bound) => value.as_f64().is_some_and(|v| v > *bound),
            Predicate::Contains(needle) => match value {
                serde_json::Value::String(s) => s.contains(needle.as_str()),
                serde_json::Value::Array(arr) => {
                    arr.iter().any(|v| v.as_str().is_some_and(|s| s == needle))
                }
                _ => false,
            },
            Predicate::NotContains(needle) => match value {
                serde_json::Value::String(s) => !s.contains(needle.as_str()),
                serde_json::Value::Array(arr) => {
                    !arr.iter().any(|v| v.as_str().is_some_and(|s| s == needle))
                }
                _ => false,
            },
            Predicate::OneOf(allowed) => allowed.contains(value),
            Predicate::ContainsAll(required) => match value {
                serde_json::Value::String(s) => required.iter().all(|r| s.contains(r.as_str())),
                serde_json::Value::Array(arr) => required
                    .iter()
                    .all(|r| arr.iter().any(|v| v.as_str().is_some_and(|s| s == r))),
                _ => false,
            },
            Predicate::Matches(regex) => value.as_str().is_some_and(|s| regex.is_match(s)),
        }
    }
}

impl CompiledTool {
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn match_source(&self) -> MatchSource {
        self.match_source
    }

    /// Check tool-level constraints against params.
    /// All must pass (conjunction). Returns false if any fail.
    pub(super) fn check_constraints(&self, params: &serde_json::Value) -> bool {
        self.constraints.iter().all(|c| c.evaluate(params))
    }

    /// Find the first matching action for a command.
    /// Actions are stored in descending privilege order (Commit first),
    /// so the highest-privilege match always wins.
    pub(super) fn match_action(&self, command: &str) -> Option<&CompiledAction> {
        self.actions.iter().find(|a| a.patterns.is_match(command))
    }

    /// Find the highest-privilege tier whose patterns match the command.
    /// Convenience wrapper around `match_action()`.
    #[cfg(test)]
    pub(super) fn match_tier(&self, command: &str) -> Option<Tier> {
        self.match_action(command).map(|a| a.tier)
    }
}

impl CompiledAction {
    /// Check action-level constraints against params.
    pub(super) fn check_constraints(&self, params: &serde_json::Value) -> bool {
        self.constraints.iter().all(|c| c.evaluate(params))
    }
}

/// Compile a single constraint config into a validated predicate.
fn compile_constraint(
    context: &str,
    config: ConstraintConfig,
) -> Result<CompiledConstraint, CherubError> {
    let predicate = match config.op {
        ConstraintOp::Eq => Predicate::Eq(config.value),
        ConstraintOp::Lt => {
            let n = config.value.as_f64().ok_or_else(|| {
                CherubError::PolicyValidation(format!(
                    "{context}, constraint on '{}': 'lt' requires a numeric value",
                    config.field
                ))
            })?;
            Predicate::Lt(n)
        }
        ConstraintOp::Gt => {
            let n = config.value.as_f64().ok_or_else(|| {
                CherubError::PolicyValidation(format!(
                    "{context}, constraint on '{}': 'gt' requires a numeric value",
                    config.field
                ))
            })?;
            Predicate::Gt(n)
        }
        ConstraintOp::Contains => {
            let s = config.value.as_str().ok_or_else(|| {
                CherubError::PolicyValidation(format!(
                    "{context}, constraint on '{}': 'contains' requires a string value",
                    config.field
                ))
            })?;
            Predicate::Contains(s.to_owned())
        }
        ConstraintOp::NotContains => {
            let s = config.value.as_str().ok_or_else(|| {
                CherubError::PolicyValidation(format!(
                    "{context}, constraint on '{}': 'not_contains' requires a string value",
                    config.field
                ))
            })?;
            Predicate::NotContains(s.to_owned())
        }
        ConstraintOp::OneOf => {
            let arr = config.value.as_array().ok_or_else(|| {
                CherubError::PolicyValidation(format!(
                    "{context}, constraint on '{}': 'one_of' requires an array value",
                    config.field
                ))
            })?;
            Predicate::OneOf(arr.clone())
        }
        ConstraintOp::ContainsAll => {
            let arr = config.value.as_array().ok_or_else(|| {
                CherubError::PolicyValidation(format!(
                    "{context}, constraint on '{}': 'contains_all' requires an array value",
                    config.field
                ))
            })?;
            let strings: Vec<String> = arr
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_owned())
                        .ok_or_else(|| {
                            CherubError::PolicyValidation(format!(
                                "{context}, constraint on '{}': 'contains_all' requires an array of strings",
                                config.field
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Predicate::ContainsAll(strings)
        }
        ConstraintOp::Matches => {
            let pattern = config.value.as_str().ok_or_else(|| {
                CherubError::PolicyValidation(format!(
                    "{context}, constraint on '{}': 'matches' requires a string value",
                    config.field
                ))
            })?;
            let regex = regex::RegexBuilder::new(pattern)
                .size_limit(1 << 20)
                .nest_limit(50)
                .unicode(false)
                .build()
                .map_err(|e| {
                    CherubError::PolicyValidation(format!(
                        "{context}, constraint on '{}': invalid regex: {e}",
                        config.field
                    ))
                })?;
            Predicate::Matches(regex)
        }
    };

    Ok(CompiledConstraint {
        field: config.field,
        predicate,
    })
}

fn compile_tool(name: String, config: ToolConfig) -> Result<CompiledTool, CherubError> {
    let tool_context = format!("tool '{name}'");

    // Compile tool-level constraints.
    let tool_constraints = config
        .constraints
        .into_iter()
        .map(|c| compile_constraint(&tool_context, c))
        .collect::<Result<Vec<_>, _>>()?;

    let mut actions: Vec<CompiledAction> = config
        .actions
        .into_iter()
        .map(|(action_name, action)| {
            if action.patterns.is_empty() {
                return Err(CherubError::PolicyValidation(format!(
                    "tool '{name}', action '{action_name}': patterns must not be empty"
                )));
            }

            let tier: Tier = action.tier.into();
            let action_context = format!("tool '{name}', action '{action_name}'");

            let patterns = regex::RegexSetBuilder::new(&action.patterns)
                .size_limit(1 << 20)
                .nest_limit(50)
                .unicode(false)
                .build()
                .map_err(|e| CherubError::PolicyValidation(format!("{action_context}: {e}")))?;

            let constraints = action
                .constraints
                .into_iter()
                .map(|c| compile_constraint(&action_context, c))
                .collect::<Result<Vec<_>, _>>()?;

            let on_constraint_failure = match action.on_constraint_failure {
                Some(OnConstraintFailureValue::Reject) | None => OnConstraintFailure::Reject,
                Some(OnConstraintFailureValue::Escalate) => OnConstraintFailure::Escalate,
            };

            Ok(CompiledAction {
                name: action_name,
                tier,
                patterns,
                constraints,
                on_constraint_failure,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Sort: highest privilege first (Commit > Act > Observe) so first match wins.
    actions.sort_by(|a, b| b.tier.cmp(&a.tier));

    Ok(CompiledTool {
        name,
        enabled: config.enabled,
        match_source: config.match_source.into(),
        actions,
        constraints: tool_constraints,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(tool.actions.len(), 3);
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

    // --- Predicate evaluation tests ---

    fn make_constraint(field: &str, predicate: Predicate) -> CompiledConstraint {
        CompiledConstraint {
            field: field.to_owned(),
            predicate,
        }
    }

    #[test]
    fn predicate_eq_string() {
        let c = make_constraint("mode", Predicate::Eq(json!("read")));
        assert!(c.evaluate(&json!({"mode": "read"})));
        assert!(!c.evaluate(&json!({"mode": "write"})));
    }

    #[test]
    fn predicate_eq_number() {
        let c = make_constraint("count", Predicate::Eq(json!(5)));
        assert!(c.evaluate(&json!({"count": 5})));
        assert!(!c.evaluate(&json!({"count": 6})));
    }

    #[test]
    fn predicate_eq_bool() {
        let c = make_constraint("dry_run", Predicate::Eq(json!(true)));
        assert!(c.evaluate(&json!({"dry_run": true})));
        assert!(!c.evaluate(&json!({"dry_run": false})));
    }

    #[test]
    fn predicate_lt() {
        let c = make_constraint("size", Predicate::Lt(100.0));
        assert!(c.evaluate(&json!({"size": 50})));
        assert!(!c.evaluate(&json!({"size": 100}))); // equal → false
        assert!(!c.evaluate(&json!({"size": 150})));
    }

    #[test]
    fn predicate_gt() {
        let c = make_constraint("size", Predicate::Gt(100.0));
        assert!(c.evaluate(&json!({"size": 150})));
        assert!(!c.evaluate(&json!({"size": 100}))); // equal → false
        assert!(!c.evaluate(&json!({"size": 50})));
    }

    #[test]
    fn predicate_lt_non_numeric() {
        let c = make_constraint("name", Predicate::Lt(100.0));
        assert!(!c.evaluate(&json!({"name": "hello"})));
    }

    #[test]
    fn predicate_gt_non_numeric() {
        let c = make_constraint("name", Predicate::Gt(100.0));
        assert!(!c.evaluate(&json!({"name": "hello"})));
    }

    #[test]
    fn predicate_contains_string() {
        let c = make_constraint("path", Predicate::Contains("/tmp".to_owned()));
        assert!(c.evaluate(&json!({"path": "/tmp/foo"})));
        assert!(!c.evaluate(&json!({"path": "/home/user"})));
    }

    #[test]
    fn predicate_contains_array() {
        let c = make_constraint("tags", Predicate::Contains("safe".to_owned()));
        assert!(c.evaluate(&json!({"tags": ["safe", "tested"]})));
        assert!(!c.evaluate(&json!({"tags": ["untested"]})));
    }

    #[test]
    fn predicate_not_contains_string() {
        let c = make_constraint("path", Predicate::NotContains("..".to_owned()));
        assert!(c.evaluate(&json!({"path": "/tmp/foo"})));
        assert!(!c.evaluate(&json!({"path": "/tmp/../etc/passwd"})));
    }

    #[test]
    fn predicate_not_contains_array() {
        let c = make_constraint("tags", Predicate::NotContains("dangerous".to_owned()));
        assert!(c.evaluate(&json!({"tags": ["safe"]})));
        assert!(!c.evaluate(&json!({"tags": ["dangerous", "safe"]})));
    }

    #[test]
    fn predicate_one_of() {
        let c = make_constraint(
            "env",
            Predicate::OneOf(vec![json!("dev"), json!("staging")]),
        );
        assert!(c.evaluate(&json!({"env": "dev"})));
        assert!(c.evaluate(&json!({"env": "staging"})));
        assert!(!c.evaluate(&json!({"env": "production"})));
    }

    #[test]
    fn predicate_contains_all_array() {
        let c = make_constraint(
            "flags",
            Predicate::ContainsAll(vec!["--dry-run".to_owned(), "--verbose".to_owned()]),
        );
        assert!(c.evaluate(&json!({"flags": ["--dry-run", "--verbose", "--color"]})));
        assert!(!c.evaluate(&json!({"flags": ["--dry-run"]})));
    }

    #[test]
    fn predicate_contains_all_string() {
        let c = make_constraint(
            "command",
            Predicate::ContainsAll(vec!["--safe".to_owned(), "--log".to_owned()]),
        );
        assert!(c.evaluate(&json!({"command": "run --safe --log output"})));
        assert!(!c.evaluate(&json!({"command": "run --safe"})));
    }

    #[test]
    fn predicate_matches() {
        let regex = regex::RegexBuilder::new(r"^/tmp/")
            .unicode(false)
            .build()
            .unwrap();
        let c = make_constraint("path", Predicate::Matches(regex));
        assert!(c.evaluate(&json!({"path": "/tmp/foo"})));
        assert!(!c.evaluate(&json!({"path": "/home/user"})));
    }

    #[test]
    fn missing_field_is_false() {
        let c = make_constraint("missing", Predicate::Eq(json!("anything")));
        assert!(!c.evaluate(&json!({"other": "value"})));
    }

    // --- Constraint parsing tests ---

    #[test]
    fn parse_tool_level_constraints() {
        let toml = r#"
[tools.bash]
enabled = true
constraints = [
    { field = "working_dir", op = "contains", value = "/tmp" },
]

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;
        let policy = Policy::from_str(toml).expect("should parse");
        let tool = policy.find_tool("bash").expect("bash should exist");
        assert_eq!(tool.constraints.len(), 1);
    }

    #[test]
    fn parse_action_level_constraints() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir "]
constraints = [
    { field = "working_dir", op = "contains", value = "/tmp" },
]
on_constraint_failure = "escalate"
"#;
        let policy = Policy::from_str(toml).expect("should parse");
        let tool = policy.find_tool("bash").expect("bash should exist");
        let action = tool.match_action("mkdir /tmp/test").expect("should match");
        assert_eq!(action.constraints.len(), 1);
        assert_eq!(action.on_constraint_failure, OnConstraintFailure::Escalate);
    }

    #[test]
    fn on_constraint_failure_default_is_reject() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir "]
constraints = [
    { field = "x", op = "eq", value = 1 },
]
"#;
        let policy = Policy::from_str(toml).expect("should parse");
        let tool = policy.find_tool("bash").expect("bash should exist");
        let action = tool.match_action("mkdir /tmp").expect("should match");
        assert_eq!(action.on_constraint_failure, OnConstraintFailure::Reject);
    }

    #[test]
    fn constraint_lt_with_string_value_rejected() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
constraints = [
    { field = "count", op = "lt", value = "not_a_number" },
]
"#;
        let err = Policy::from_str(toml).unwrap_err();
        assert!(matches!(err, CherubError::PolicyValidation(_)));
    }

    #[test]
    fn constraint_one_of_with_non_array_rejected() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
constraints = [
    { field = "env", op = "one_of", value = "not_an_array" },
]
"#;
        let err = Policy::from_str(toml).unwrap_err();
        assert!(matches!(err, CherubError::PolicyValidation(_)));
    }

    #[test]
    fn constraint_invalid_matches_regex_rejected() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
constraints = [
    { field = "path", op = "matches", value = "[invalid" },
]
"#;
        let err = Policy::from_str(toml).unwrap_err();
        assert!(matches!(err, CherubError::PolicyValidation(_)));
    }

    #[test]
    fn constraint_unknown_field_in_constraint_rejected() {
        let toml = r#"
[tools.bash]
enabled = true
constraints = [
    { field = "x", op = "eq", value = 1, extra = "bad" },
]

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;
        let err = Policy::from_str(toml).unwrap_err();
        assert!(matches!(err, CherubError::PolicyLoad(_)));
    }

    #[test]
    fn backward_compat_no_constraints() {
        // Existing TOML without constraints still parses.
        let policy = Policy::from_str(DEFAULT_POLICY).expect("should parse");
        let tool = policy.find_tool("bash").expect("bash should exist");
        assert!(tool.constraints.is_empty());
        for action in &tool.actions {
            assert!(action.constraints.is_empty());
            assert_eq!(action.on_constraint_failure, OnConstraintFailure::Reject);
        }
    }

    #[test]
    fn tool_constraints_check() {
        let toml = r#"
[tools.bash]
enabled = true
constraints = [
    { field = "working_dir", op = "contains", value = "/tmp" },
]

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;
        let policy = Policy::from_str(toml).expect("should parse");
        let tool = policy.find_tool("bash").unwrap();
        assert!(tool.check_constraints(&json!({"command": "ls /tmp", "working_dir": "/tmp/foo"})));
        assert!(!tool.check_constraints(&json!({"command": "ls /tmp", "working_dir": "/home"})));
    }

    // --- Step 6: Policy loading error handling ---

    #[test]
    fn truncated_toml_fails() {
        let incomplete = "[tools.bash]\nenabled = tr"; // truncated boolean
        assert!(Policy::from_str(incomplete).is_err());
    }

    #[test]
    fn binary_content_fails() {
        let binary = "\x00\x01\x02\x03";
        assert!(Policy::from_str(binary).is_err());
    }

    #[test]
    fn policy_file_size_limit_constant() {
        assert_eq!(MAX_POLICY_FILE_SIZE, 64 * 1024);
    }
}
