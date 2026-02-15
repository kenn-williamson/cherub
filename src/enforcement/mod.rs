pub mod capability;
pub mod policy;
pub mod tier;

use capability::CapabilityToken;
use policy::Policy;
use tier::Tier;
use crate::tools::{Evaluated, Proposed, ToolInvocation};

/// Result of enforcement evaluation.
pub enum Decision {
    Allow(CapabilityToken),
    Reject,
    Escalate,
}

/// Evaluate a proposed tool invocation against the policy.
///
/// Returns the transitioned invocation (now `Evaluated`) and the decision.
/// Matching algorithm checks tiers in descending privilege order (Commit first)
/// so the highest-privilege match always wins.
pub fn evaluate(
    proposal: ToolInvocation<Proposed>,
    policy: &Policy,
) -> (ToolInvocation<Evaluated>, Decision) {
    let decision = match extract_command(&proposal.params) {
        None => Decision::Reject,
        Some(command) => match policy.find_tool(&proposal.tool) {
            None => Decision::Reject,
            Some(tool) if !tool.enabled() => Decision::Reject,
            Some(tool) => match tool.match_tier(command) {
                Some(Tier::Commit) => Decision::Escalate,
                Some(tier) => Decision::Allow(CapabilityToken::new(tier)),
                None => Decision::Reject,
            },
        },
    };

    (proposal.transition(), decision)
}

/// Extract `params["command"]` as a non-empty string.
fn extract_command(params: &serde_json::Value) -> Option<&str> {
    params
        .get("command")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
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

    fn make_proposal(tool: &str, command: &str) -> ToolInvocation<Proposed> {
        ToolInvocation::new(tool, "execute", json!({"command": command}))
    }

    fn make_proposal_no_command(tool: &str) -> ToolInvocation<Proposed> {
        ToolInvocation::new(tool, "execute", json!({"args": ["--version"]}))
    }

    #[test]
    fn observe_command_allowed() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn act_command_allowed() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "mkdir /tmp/test"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Act),
            _ => panic!("expected Allow(Act)"),
        }
    }

    #[test]
    fn commit_command_escalates() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "rm -rf /tmp/test"), &policy);
        assert!(matches!(decision, Decision::Escalate));
    }

    #[test]
    fn unmatched_command_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "curl http://evil.com"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn empty_command_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", ""), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn unknown_tool_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("python", "print('hi')"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn disabled_tool_rejected() {
        let toml = r#"
[tools.bash]
enabled = false

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;
        let policy = Policy::from_str(toml).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn empty_policy_rejects_all() {
        let policy = Policy::from_str("[tools]\n").unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn missing_command_param_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal_no_command("bash"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn highest_privilege_wins() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "sudo ls /tmp"), &policy);
        assert!(matches!(decision, Decision::Escalate));
    }

    #[test]
    fn exact_match_pwd() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "pwd"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }
}
