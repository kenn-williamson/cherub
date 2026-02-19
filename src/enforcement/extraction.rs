//! Action extraction strategies for the enforcement layer.
//!
//! Different tools express their intended action differently:
//! - `bash` puts it in `params["command"]`, parsed via the shell module
//! - `memory` puts it in `params["action"]`, optionally qualified by `params["path"]`
//!
//! `MatchSource` selects the extraction strategy at policy-compile time.
//! No changes to `evaluate()` are needed when adding new structured tools.

use super::shell;

/// How to extract matchable action strings from a tool invocation's params.
///
/// `Copy` so it can be stored on `CompiledTool` without affecting cloneability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MatchSource {
    /// Extract `params["command"]`, parse via the shell module.
    /// Each sub-command (split on `;`, `&&`, `|`, etc.) becomes a separate action string.
    Command,
    /// Extract `params["action"]`, optionally qualified by `params["path"]`.
    /// Produces a single action string: `"{action}:{path}"` or `"{action}"`.
    Structured,
}

impl MatchSource {
    /// Extract matchable action strings from tool invocation params.
    ///
    /// Returns `None` if the params are malformed or unparseable (→ Reject).
    /// Returns `Some([])` is never produced — an empty list is treated as `None`.
    pub(super) fn extract(self, params: &serde_json::Value) -> Option<Vec<String>> {
        match self {
            MatchSource::Command => {
                let command = params
                    .get("command")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())?;

                let sub_commands = shell::parse_commands(command)?;

                if sub_commands.is_empty() {
                    return None;
                }

                Some(sub_commands.iter().map(|s| s.to_string()).collect())
            }
            MatchSource::Structured => {
                let action = params
                    .get("action")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())?;

                let action_str = match params.get("path").and_then(|v| v.as_str()) {
                    Some(path) if !path.is_empty() => format!("{action}:{path}"),
                    _ => action.to_owned(),
                };

                Some(vec![action_str])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Command extraction ---

    #[test]
    fn command_simple() {
        let params = json!({"command": "ls /tmp"});
        assert_eq!(
            MatchSource::Command.extract(&params),
            Some(vec!["ls /tmp".to_owned()])
        );
    }

    #[test]
    fn command_compound_splits() {
        let params = json!({"command": "ls /tmp; pwd"});
        let actions = MatchSource::Command.extract(&params).unwrap();
        assert_eq!(actions, vec!["ls /tmp", "pwd"]);
    }

    #[test]
    fn command_missing_returns_none() {
        let params = json!({"args": ["ls"]});
        assert!(MatchSource::Command.extract(&params).is_none());
    }

    #[test]
    fn command_empty_string_returns_none() {
        let params = json!({"command": ""});
        assert!(MatchSource::Command.extract(&params).is_none());
    }

    #[test]
    fn command_unparseable_returns_none() {
        // Here-doc is unparseable.
        let params = json!({"command": "cat <<EOF\nhello\nEOF"});
        assert!(MatchSource::Command.extract(&params).is_none());
    }

    #[test]
    fn command_non_string_returns_none() {
        let params = json!({"command": 42});
        assert!(MatchSource::Command.extract(&params).is_none());
    }

    // --- Structured extraction ---

    #[test]
    fn structured_action_only() {
        let params = json!({"action": "recall"});
        assert_eq!(
            MatchSource::Structured.extract(&params),
            Some(vec!["recall".to_owned()])
        );
    }

    #[test]
    fn structured_action_with_path() {
        let params = json!({"action": "store", "path": "preferences/food"});
        assert_eq!(
            MatchSource::Structured.extract(&params),
            Some(vec!["store:preferences/food".to_owned()])
        );
    }

    #[test]
    fn structured_empty_path_omitted() {
        let params = json!({"action": "search", "path": ""});
        assert_eq!(
            MatchSource::Structured.extract(&params),
            Some(vec!["search".to_owned()])
        );
    }

    #[test]
    fn structured_missing_action_returns_none() {
        let params = json!({"path": "preferences/food"});
        assert!(MatchSource::Structured.extract(&params).is_none());
    }

    #[test]
    fn structured_empty_action_returns_none() {
        let params = json!({"action": ""});
        assert!(MatchSource::Structured.extract(&params).is_none());
    }

    #[test]
    fn structured_non_string_action_returns_none() {
        let params = json!({"action": 42});
        assert!(MatchSource::Structured.extract(&params).is_none());
    }
}
