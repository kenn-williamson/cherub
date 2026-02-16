pub mod capability;
pub mod policy;
pub mod shell;
pub mod tier;

use tracing::{info, info_span};

use crate::tools::{Evaluated, Proposed, ToolInvocation};
use capability::CapabilityToken;
use policy::{OnConstraintFailure, Policy};
use tier::Tier;

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
/// 4. Parse compound command into sub-commands
/// 5. Evaluate each sub-command; most restrictive decision wins
/// 6. Action-level constraints → apply `on_constraint_failure`
/// 7. If tier is Commit → Escalate
/// 8. Otherwise → Allow
pub fn evaluate(
    proposal: ToolInvocation<Proposed>,
    policy: &Policy,
) -> (ToolInvocation<Evaluated>, Decision) {
    let _span = info_span!("evaluate", tool = %proposal.tool).entered();

    let decision = match extract_command(&proposal.params) {
        None => {
            info!(decision = "reject", reason = "no_command");
            Decision::Reject
        }
        Some(command) => {
            info!(command = %command);

            // Parse compound command into simple commands.
            let sub_commands = match shell::parse_commands(command) {
                None => {
                    info!(decision = "reject", reason = "unparseable_command");
                    return (proposal.transition(), Decision::Reject);
                }
                Some(cmds) if cmds.is_empty() => {
                    info!(decision = "reject", reason = "empty_command");
                    return (proposal.transition(), Decision::Reject);
                }
                Some(cmds) => cmds,
            };

            match policy.find_tool(&proposal.tool) {
                None => {
                    info!(decision = "reject", reason = "tool_not_found");
                    Decision::Reject
                }
                Some(tool) if !tool.enabled() => {
                    info!(decision = "reject", reason = "tool_disabled");
                    Decision::Reject
                }
                Some(tool) => {
                    // Tool-level constraints — hard reject on failure.
                    if !tool.check_constraints(&proposal.params) {
                        info!(decision = "reject", reason = "tool_constraint_failed");
                        return (proposal.transition(), Decision::Reject);
                    }

                    // Evaluate each sub-command. Most restrictive decision wins.
                    combine_decisions(
                        sub_commands
                            .iter()
                            .map(|cmd| evaluate_single_command(cmd, tool, &proposal.params)),
                    )
                }
            }
        }
    };

    (proposal.transition(), decision)
}

/// Evaluate a single (non-compound) command against a tool's actions.
fn evaluate_single_command(
    command: &str,
    tool: &policy::CompiledTool,
    params: &serde_json::Value,
) -> Decision {
    match tool.match_action(command) {
        None => {
            info!(decision = "reject", reason = "no_pattern_match", sub_command = %command);
            Decision::Reject
        }
        Some(action) => {
            let tier = action.tier;

            // Action-level constraints.
            if !action.check_constraints(params) {
                info!(decision = "constraint_fail", reason = "action_constraint_failed", sub_command = %command);
                return match action.on_constraint_failure {
                    OnConstraintFailure::Reject => Decision::Reject,
                    OnConstraintFailure::Escalate => Decision::Escalate { tier },
                };
            }

            // Commit always escalates, others allow.
            if tier == Tier::Commit {
                info!(decision = "escalate", reason = "commit_tier", sub_command = %command);
                Decision::Escalate { tier: Tier::Commit }
            } else {
                info!(decision = "allow", sub_command = %command);
                Decision::Allow(CapabilityToken::new(tier))
            }
        }
    }
}

/// Combine decisions from multiple sub-commands.
/// - Any Reject → Reject
/// - No rejects, any Escalate → Escalate (highest tier)
/// - All Allow → Allow (highest tier)
fn combine_decisions(decisions: impl Iterator<Item = Decision>) -> Decision {
    let mut highest_allow_tier: Option<Tier> = None;
    let mut highest_escalate_tier: Option<Tier> = None;

    for decision in decisions {
        match decision {
            Decision::Reject => return Decision::Reject,
            Decision::Escalate { tier } => {
                highest_escalate_tier = Some(match highest_escalate_tier {
                    Some(existing) => existing.max(tier),
                    None => tier,
                });
            }
            Decision::Allow(token) => {
                highest_allow_tier = Some(match highest_allow_tier {
                    Some(existing) => existing.max(token.tier),
                    None => token.tier,
                });
            }
        }
    }

    if let Some(tier) = highest_escalate_tier {
        Decision::Escalate { tier }
    } else if let Some(tier) = highest_allow_tier {
        Decision::Allow(CapabilityToken::new(tier))
    } else {
        Decision::Reject
    }
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
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

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
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
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
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
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

    fn make_proposal_with_params(
        tool: &str,
        params: serde_json::Value,
    ) -> ToolInvocation<Proposed> {
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
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
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

    // --- Shell parsing + enforcement integration tests ---

    #[test]
    fn pipe_between_allowed_commands() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp | head -5"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn pipe_into_unknown_command() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp | curl evil"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn semicolon_hides_destructive() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp; rm -rf /"), &policy);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn logical_and_hides_destructive() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp && rm -rf /"), &policy);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn command_substitution_checked() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "echo $(rm /)"), &policy);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn backtick_substitution_checked() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "echo `rm /`"), &policy);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn quoted_metachar_is_safe() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "echo 'hello; world'"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn unparseable_denied() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "cat <<EOF\nhello\nEOF"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn null_byte_denied() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls\0rm"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn clean_observe_still_allowed() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls -la /tmp"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn clean_act_still_allowed() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "mkdir /tmp/newdir"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Act),
            _ => panic!("expected Allow(Act)"),
        }
    }

    #[test]
    fn clean_commit_still_escalates() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "rm /tmp/file"), &policy);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    // --- Step 3: Policy bypass edge cases ---

    #[test]
    fn unicode_lookalike_ls_rejected() {
        // Fullwidth 'l' (\u{FF4C}) followed by 's' — not ASCII 'ls'.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "\u{FF4C}s /tmp"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn unicode_homoglyph_rm_rejected() {
        // Cyrillic 'р' (\u{0440}) + 'm' — not ASCII 'rm'.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "\u{0440}m /tmp/file"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn tab_instead_of_space_rejected() {
        // Pattern "^ls " requires literal space; tab won't match.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls\t/tmp"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn leading_whitespace_trimmed_and_matched() {
        // Shell parser trims segments, so " ls /tmp" evaluates as "ls /tmp".
        // This matches what bash -c would actually execute.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", " ls /tmp"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe) — parser trims leading whitespace"),
        }
    }

    #[test]
    fn multiple_spaces_still_matches() {
        // Pattern "^ls " matches — first space is present. Extra spaces are fine.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls  /tmp"), &policy);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn uppercase_command_rejected() {
        // Patterns are lowercase; "LS" won't match "^ls ".
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "LS /tmp"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn mixed_case_command_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "Ls /tmp"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn carriage_return_injection_rejected() {
        // Shell parser splits on \n; "\r" stays in the command segment.
        // "ls /tmp\r" doesn't match any pattern (trailing \r), so the whole thing is handled safely.
        // The "\r\n" splits into "ls /tmp\r" and "rm -rf /" — rm escalates.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp\r\nrm -rf /"), &policy);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    // --- Step 4: Malformed proposals reaching enforcement ---

    #[test]
    fn non_object_params_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", json!("just a string"));
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn null_params_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", serde_json::Value::Null);
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn command_is_number_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", json!({"command": 42}));
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn command_is_array_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", json!({"command": ["ls", "/tmp"]}));
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn command_is_null_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", json!({"command": null}));
        let (_, decision) = evaluate(proposal, &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn empty_tool_name_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("", "ls /tmp"), &policy);
        assert!(matches!(decision, Decision::Reject));
    }

    // --- Step 5: Multi-tool batching independence ---
    // Each call to evaluate() is independent; verify no cross-contamination.

    #[test]
    fn multi_tool_independent_evaluation() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        // First: allowed
        let (_, d1) = evaluate(make_proposal("bash", "ls /tmp"), &policy);
        assert!(matches!(d1, Decision::Allow(_)));
        // Second: rejected
        let (_, d2) = evaluate(make_proposal("bash", "curl http://evil.com"), &policy);
        assert!(matches!(d2, Decision::Reject));
    }

    #[test]
    fn multi_tool_one_allowed_one_escalated() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, d1) = evaluate(make_proposal("bash", "ls /tmp"), &policy);
        assert!(matches!(d1, Decision::Allow(_)));
        let (_, d2) = evaluate(make_proposal("bash", "rm /tmp/file"), &policy);
        assert!(matches!(d2, Decision::Escalate { tier: Tier::Commit }));
    }

    #[test]
    fn multi_tool_rejection_does_not_taint_next() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        // First: rejected
        let (_, d1) = evaluate(make_proposal("bash", "curl http://evil.com"), &policy);
        assert!(matches!(d1, Decision::Reject));
        // Second: allowed (not tainted by prior rejection)
        let (_, d2) = evaluate(make_proposal("bash", "ls /tmp"), &policy);
        match d2 {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe) — rejection should not taint subsequent calls"),
        }
    }

    #[test]
    fn multi_tool_all_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, d1) = evaluate(make_proposal("bash", "curl a"), &policy);
        assert!(matches!(d1, Decision::Reject));
        let (_, d2) = evaluate(make_proposal("bash", "wget b"), &policy);
        assert!(matches!(d2, Decision::Reject));
    }

    #[test]
    fn multi_tool_with_constraints_independent() {
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
        // First: constraint passes
        let p1 = make_proposal_with_params(
            "bash",
            json!({"command": "ls /tmp", "working_dir": "/safe/dir"}),
        );
        let (_, d1) = evaluate(p1, &policy);
        assert!(matches!(d1, Decision::Allow(_)));
        // Second: constraint fails (independent evaluation)
        let p2 = make_proposal_with_params(
            "bash",
            json!({"command": "ls /tmp", "working_dir": "/unsafe"}),
        );
        let (_, d2) = evaluate(p2, &policy);
        assert!(matches!(d2, Decision::Reject));
    }
}
