pub mod capability;
pub mod policy;
pub mod tier;

use capability::CapabilityToken;
use policy::{OnConstraintFailure, Policy};
use tier::Tier;
use crate::tools::{Evaluated, Proposed, ToolInvocation};

/// Result of enforcement evaluation.
pub enum Decision {
    Allow(CapabilityToken),
    Reject,
    Escalate { tier: Tier },
}

/// Issue a CapabilityToken for a human-approved escalation.
/// Only code path that creates tokens for escalated actions.
pub fn approve_escalation(tier: Tier) -> CapabilityToken {
    CapabilityToken::new(tier)
}

/// Evaluate a proposed tool invocation against the policy.
///
/// Returns the transitioned invocation (now `Evaluated`) and the decision.
///
/// Flow:
/// 1. Extract command from params
/// 2. Find tool in policy, check enabled
/// 3. Tool-level constraints → hard reject on failure
/// 4. Match action via `tool.match_action(command)`
/// 5. Action-level constraints → apply `on_constraint_failure`
/// 6. If tier is Commit → Escalate
/// 7. Otherwise → Allow
pub fn evaluate(
    proposal: ToolInvocation<Proposed>,
    policy: &Policy,
) -> (ToolInvocation<Evaluated>, Decision) {
    let decision = match extract_command(&proposal.params) {
        None => Decision::Reject,
        Some(command) => match policy.find_tool(&proposal.tool) {
            None => Decision::Reject,
            Some(tool) if !tool.enabled() => Decision::Reject,
            Some(tool) => {
                // Step 3: Tool-level constraints — hard reject on failure.
                if !tool.check_constraints(&proposal.params) {
                    return (proposal.transition(), Decision::Reject);
                }

                // Step 4: Match action.
                match tool.match_action(command) {
                    None => Decision::Reject,
                    Some(action) => {
                        let tier = action.tier;

                        // Step 5: Action-level constraints.
                        if !action.check_constraints(&proposal.params) {
                            return (
                                proposal.transition(),
                                match action.on_constraint_failure {
                                    OnConstraintFailure::Reject => Decision::Reject,
                                    OnConstraintFailure::Escalate => {
                                        Decision::Escalate { tier }
                                    }
                                },
                            );
                        }

                        // Step 6/7: Commit always escalates, others allow.
                        if tier == Tier::Commit {
                            Decision::Escalate { tier: Tier::Commit }
                        } else {
                            Decision::Allow(CapabilityToken::new(tier))
                        }
                    }
                }
            }
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
        assert!(matches!(decision, Decision::Escalate { tier: Tier::Commit }));
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
        assert!(matches!(decision, Decision::Escalate { tier: Tier::Commit }));
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

    // --- Constraint + enforcement integration tests ---

    fn make_proposal_with_params(tool: &str, params: serde_json::Value) -> ToolInvocation<Proposed> {
        ToolInvocation::new(tool, "execute", params)
    }

    #[test]
    fn tool_constraint_failure_rejects() {
        let toml = r#"
[tools.bash]
enabled = true
constraints = [
    { field = "working_dir", op = "contains", value = "/safe" },
]

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;
        let policy = Policy::from_str(toml).unwrap();
        let proposal = make_proposal_with_params(
            "bash",
            json!({"command": "ls /tmp", "working_dir": "/unsafe/path"}),
        );
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn tool_constraint_pass_allows() {
        let toml = r#"
[tools.bash]
enabled = true
constraints = [
    { field = "working_dir", op = "contains", value = "/safe" },
]

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]
"#;
        let policy = Policy::from_str(toml).unwrap();
        let proposal = make_proposal_with_params(
            "bash",
            json!({"command": "ls /tmp", "working_dir": "/safe/dir"}),
        );
        let (_, decision) = evaluate(proposal, &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn action_constraint_failure_reject() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir "]
constraints = [
    { field = "command", op = "not_contains", value = ".." },
]
on_constraint_failure = "reject"
"#;
        let policy = Policy::from_str(toml).unwrap();
        let proposal = make_proposal("bash", "mkdir ../escape");
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn action_constraint_failure_escalate() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir "]
constraints = [
    { field = "command", op = "not_contains", value = ".." },
]
on_constraint_failure = "escalate"
"#;
        let policy = Policy::from_str(toml).unwrap();
        let proposal = make_proposal("bash", "mkdir ../escape");
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Escalate { tier: Tier::Act }));
    }

    #[test]
    fn commit_tier_always_escalates_even_with_passing_constraints() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.destructive]
tier = "commit"
patterns = ["^rm "]
constraints = [
    { field = "command", op = "contains", value = "/tmp" },
]
"#;
        let policy = Policy::from_str(toml).unwrap();
        let proposal = make_proposal("bash", "rm /tmp/test");
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Escalate { tier: Tier::Commit }));
    }

    #[test]
    fn all_constraints_pass_act_tier_allows() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.write]
tier = "act"
patterns = ["^mkdir "]
constraints = [
    { field = "command", op = "not_contains", value = ".." },
]
"#;
        let policy = Policy::from_str(toml).unwrap();
        let proposal = make_proposal("bash", "mkdir /tmp/safe");
        let (_, decision) = evaluate(proposal, &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Act),
            _ => panic!("expected Allow(Act)"),
        }
    }

    #[test]
    fn no_constraints_preserves_existing_behavior() {
        // No constraints → same behavior as before M3.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();

        // Observe → Allow
        let (_, d) = evaluate(make_proposal("bash", "ls /tmp"), &policy);
        assert!(matches!(d, Decision::Allow(_)));

        // Act → Allow
        let (_, d) = evaluate(make_proposal("bash", "mkdir /tmp/x"), &policy);
        assert!(matches!(d, Decision::Allow(_)));

        // Commit → Escalate
        let (_, d) = evaluate(make_proposal("bash", "rm /tmp/x"), &policy);
        assert!(matches!(d, Decision::Escalate { .. }));

        // Unknown → Reject
        let (_, d) = evaluate(make_proposal("bash", "curl http://evil.com"), &policy);
        assert!(matches!(d, Decision::Reject));
    }

    #[test]
    fn approve_escalation_creates_token() {
        let token = approve_escalation(Tier::Commit);
        assert_eq!(token.tier, Tier::Commit);

        let token = approve_escalation(Tier::Act);
        assert_eq!(token.tier, Tier::Act);
    }
}
