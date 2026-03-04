pub mod capability;
pub(crate) mod extraction;
pub mod policy;
pub mod shell;
pub mod tier;

use tracing::{info, info_span};

use crate::tools::{Evaluated, Proposed, ToolInvocation};
use capability::CapabilityToken;
use policy::{CompiledBudget, OnConstraintFailure, Policy};
use tier::Tier;

/// Runtime budget state passed into enforcement from the agent loop.
/// The enforcement layer does not own this data — it receives it as context.
#[derive(Debug)]
pub struct BudgetContext {
    pub session_cost_usd: f64,
    pub daily_cost_usd: f64,
}

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
/// 0. Budget check (if configured and context provided) — runs first
/// 1. Find tool in policy, check enabled
/// 2. Tool-level constraints → hard reject on failure
/// 3. Extract action strings via the tool's MatchSource strategy
/// 4. Evaluate each action; most restrictive decision wins
/// 5. If tier is Commit → Escalate; otherwise → Allow
pub fn evaluate(
    proposal: ToolInvocation<Proposed>,
    policy: &Policy,
    budget: Option<&BudgetContext>,
) -> (ToolInvocation<Evaluated>, Decision) {
    let _span = info_span!("evaluate", tool = %proposal.tool).entered();

    // Budget check runs first, before tool lookup. If exceeded, the response
    // depends on on_exceeded policy: escalate (human decides) or reject.
    // Policy opacity preserved — the agent sees "action not permitted", nothing more.
    if let Some(budget_ctx) = budget
        && let Some(ref compiled_budget) = policy.budget
        && let Some(decision) = check_budget(budget_ctx, compiled_budget)
    {
        return (proposal.transition(), decision);
    }

    let decision = match policy.find_tool(&proposal.tool) {
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

            // Extract action strings via the tool's configured strategy.
            match tool.match_source().extract(&proposal.params) {
                None => {
                    info!(decision = "reject", reason = "action_extraction_failed");
                    return (proposal.transition(), Decision::Reject);
                }
                Some(actions) if actions.is_empty() => {
                    info!(decision = "reject", reason = "empty_actions");
                    return (proposal.transition(), Decision::Reject);
                }
                Some(actions) => {
                    // Evaluate each action. Most restrictive decision wins.
                    combine_decisions(
                        actions
                            .iter()
                            .map(|action| evaluate_single_action(action, tool, &proposal.params)),
                    )
                }
            }
        }
    };

    (proposal.transition(), decision)
}

/// Evaluate a single action string against a tool's actions.
fn evaluate_single_action(
    action: &str,
    tool: &policy::CompiledTool,
    params: &serde_json::Value,
) -> Decision {
    match tool.match_action(action) {
        None => {
            info!(decision = "reject", reason = "no_pattern_match", action = %action);
            Decision::Reject
        }
        Some(matched_action) => {
            let tier = matched_action.tier;

            // Action-level constraints.
            if !matched_action.check_constraints(params) {
                info!(decision = "constraint_fail", reason = "action_constraint_failed", action = %action);
                return match matched_action.on_constraint_failure {
                    OnConstraintFailure::Reject => Decision::Reject,
                    OnConstraintFailure::Escalate => Decision::Escalate { tier },
                };
            }

            // Commit always escalates, others allow.
            if tier == Tier::Commit {
                info!(decision = "escalate", reason = "commit_tier", action = %action);
                Decision::Escalate { tier: Tier::Commit }
            } else {
                info!(decision = "allow", action = %action);
                Decision::Allow(CapabilityToken::new(tier))
            }
        }
    }
}

/// Check budget limits. Returns `Some(Decision)` if budget is exceeded, `None` if within budget.
fn check_budget(ctx: &BudgetContext, budget: &CompiledBudget) -> Option<Decision> {
    let session_exceeded = budget
        .session_limit_usd
        .is_some_and(|limit| ctx.session_cost_usd >= limit);
    let daily_exceeded = budget
        .daily_limit_usd
        .is_some_and(|limit| ctx.daily_cost_usd >= limit);

    if session_exceeded || daily_exceeded {
        let reason = if session_exceeded {
            "session_budget_exceeded"
        } else {
            "daily_budget_exceeded"
        };
        info!(decision = "budget_exceeded", reason);
        return match budget.on_exceeded {
            OnConstraintFailure::Escalate => Some(Decision::Escalate { tier: Tier::Commit }),
            OnConstraintFailure::Reject => Some(Decision::Reject),
        };
    }

    None
}

/// Combine decisions from multiple actions.
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
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn act_command_allowed() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "mkdir /tmp/test"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Act),
            _ => panic!("expected Allow(Act)"),
        }
    }

    #[test]
    fn commit_command_escalates() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "rm -rf /tmp/test"), &policy, None);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn unmatched_command_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "curl http://evil.com"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn empty_command_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", ""), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn unknown_tool_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("python", "print('hi')"), &policy, None);
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
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn empty_policy_rejects_all() {
        let policy = Policy::from_str("[tools]\n").unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn missing_command_param_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal_no_command("bash"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn highest_privilege_wins() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "sudo ls /tmp"), &policy, None);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn exact_match_pwd() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "pwd"), &policy, None);
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
        let (_, decision) = evaluate(proposal, &policy, None);
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
        let (_, decision) = evaluate(proposal, &policy, None);
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
        let (_, decision) = evaluate(proposal, &policy, None);
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
        let (_, decision) = evaluate(proposal, &policy, None);
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
        let (_, decision) = evaluate(proposal, &policy, None);
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
        let (_, decision) = evaluate(proposal, &policy, None);
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
        let (_, d) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        assert!(matches!(d, Decision::Allow(_)));

        // Act → Allow
        let (_, d) = evaluate(make_proposal("bash", "mkdir /tmp/x"), &policy, None);
        assert!(matches!(d, Decision::Allow(_)));

        // Commit → Escalate
        let (_, d) = evaluate(make_proposal("bash", "rm /tmp/x"), &policy, None);
        assert!(matches!(d, Decision::Escalate { .. }));

        // Unknown → Reject
        let (_, d) = evaluate(make_proposal("bash", "curl http://evil.com"), &policy, None);
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
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp | head -5"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn pipe_into_unknown_command() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp | curl evil"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn semicolon_hides_destructive() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp; rm -rf /"), &policy, None);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn logical_and_hides_destructive() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp && rm -rf /"), &policy, None);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn command_substitution_checked() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "echo $(rm /)"), &policy, None);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn backtick_substitution_checked() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "echo `rm /`"), &policy, None);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn quoted_metachar_is_safe() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "echo 'hello; world'"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn unparseable_denied() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(
            make_proposal("bash", "cat <<EOF\nhello\nEOF"),
            &policy,
            None,
        );
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn null_byte_denied() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls\0rm"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn clean_observe_still_allowed() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls -la /tmp"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn clean_act_still_allowed() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "mkdir /tmp/newdir"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Act),
            _ => panic!("expected Allow(Act)"),
        }
    }

    #[test]
    fn clean_commit_still_escalates() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "rm /tmp/file"), &policy, None);
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
        let (_, decision) = evaluate(make_proposal("bash", "\u{FF4C}s /tmp"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn unicode_homoglyph_rm_rejected() {
        // Cyrillic 'р' (\u{0440}) + 'm' — not ASCII 'rm'.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "\u{0440}m /tmp/file"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn tab_instead_of_space_rejected() {
        // Pattern "^ls " requires literal space; tab won't match.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls\t/tmp"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn leading_whitespace_trimmed_and_matched() {
        // Shell parser trims segments, so " ls /tmp" evaluates as "ls /tmp".
        // This matches what bash -c would actually execute.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", " ls /tmp"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe) — parser trims leading whitespace"),
        }
    }

    #[test]
    fn multiple_spaces_still_matches() {
        // Pattern "^ls " matches — first space is present. Extra spaces are fine.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls  /tmp"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn uppercase_command_rejected() {
        // Patterns are lowercase; "LS" won't match "^ls ".
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "LS /tmp"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn mixed_case_command_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "Ls /tmp"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn carriage_return_injection_rejected() {
        // Shell parser splits on \n; "\r" stays in the command segment.
        // "ls /tmp\r" doesn't match any pattern (trailing \r), so the whole thing is handled safely.
        // The "\r\n" splits into "ls /tmp\r" and "rm -rf /" — rm escalates.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp\r\nrm -rf /"), &policy, None);
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
        let (_, decision) = evaluate(proposal, &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn null_params_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", serde_json::Value::Null);
        let (_, decision) = evaluate(proposal, &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn command_is_number_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", json!({"command": 42}));
        let (_, decision) = evaluate(proposal, &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn command_is_array_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", json!({"command": ["ls", "/tmp"]}));
        let (_, decision) = evaluate(proposal, &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn command_is_null_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let proposal = make_proposal_with_params("bash", json!({"command": null}));
        let (_, decision) = evaluate(proposal, &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn empty_tool_name_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, decision) = evaluate(make_proposal("", "ls /tmp"), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    // --- Step 5: Multi-tool batching independence ---
    // Each call to evaluate() is independent; verify no cross-contamination.

    #[test]
    fn multi_tool_independent_evaluation() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        // First: allowed
        let (_, d1) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        assert!(matches!(d1, Decision::Allow(_)));
        // Second: rejected
        let (_, d2) = evaluate(make_proposal("bash", "curl http://evil.com"), &policy, None);
        assert!(matches!(d2, Decision::Reject));
    }

    #[test]
    fn multi_tool_one_allowed_one_escalated() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, d1) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        assert!(matches!(d1, Decision::Allow(_)));
        let (_, d2) = evaluate(make_proposal("bash", "rm /tmp/file"), &policy, None);
        assert!(matches!(d2, Decision::Escalate { tier: Tier::Commit }));
    }

    #[test]
    fn multi_tool_rejection_does_not_taint_next() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        // First: rejected
        let (_, d1) = evaluate(make_proposal("bash", "curl http://evil.com"), &policy, None);
        assert!(matches!(d1, Decision::Reject));
        // Second: allowed (not tainted by prior rejection)
        let (_, d2) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        match d2 {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe) — rejection should not taint subsequent calls"),
        }
    }

    #[test]
    fn multi_tool_all_rejected() {
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let (_, d1) = evaluate(make_proposal("bash", "curl a"), &policy, None);
        assert!(matches!(d1, Decision::Reject));
        let (_, d2) = evaluate(make_proposal("bash", "wget b"), &policy, None);
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
        let (_, d1) = evaluate(p1, &policy, None);
        assert!(matches!(d1, Decision::Allow(_)));
        // Second: constraint fails (independent evaluation)
        let p2 = make_proposal_with_params(
            "bash",
            json!({"command": "ls /tmp", "working_dir": "/unsafe"}),
        );
        let (_, d2) = evaluate(p2, &policy, None);
        assert!(matches!(d2, Decision::Reject));
    }

    // --- Structured match_source tests ---

    const MEMORY_POLICY: &str = r#"
[tools.memory]
enabled = true
match_source = "structured"

[tools.memory.actions.read]
tier = "observe"
patterns = ["^(recall|search)$", "^(recall|search):"]

[tools.memory.actions.write_user]
tier = "act"
patterns = ["^store:preferences/", "^store:observations/", "^update:preferences/", "^update:observations/"]

[tools.memory.actions.write_identity]
tier = "commit"
patterns = ["^store:identity/", "^update:identity/", "^(store|update):agent/", "^(store|update):instructions/"]

[tools.memory.actions.delete]
tier = "commit"
patterns = ["^forget"]
"#;

    fn make_memory_proposal(action: &str, path: Option<&str>) -> ToolInvocation<Proposed> {
        let params = if let Some(p) = path {
            json!({"action": action, "path": p})
        } else {
            json!({"action": action})
        };
        ToolInvocation::new("memory", "execute", params)
    }

    #[test]
    fn structured_recall_allowed_observe() {
        let policy = Policy::from_str(MEMORY_POLICY).unwrap();
        let (_, decision) = evaluate(make_memory_proposal("recall", None), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe)"),
        }
    }

    #[test]
    fn structured_store_preferences_act() {
        let policy = Policy::from_str(MEMORY_POLICY).unwrap();
        let (_, decision) = evaluate(
            make_memory_proposal("store", Some("preferences/food")),
            &policy,
            None,
        );
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Act),
            _ => panic!("expected Allow(Act)"),
        }
    }

    #[test]
    fn structured_store_identity_escalates() {
        let policy = Policy::from_str(MEMORY_POLICY).unwrap();
        let (_, decision) = evaluate(
            make_memory_proposal("store", Some("identity/values")),
            &policy,
            None,
        );
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn structured_forget_escalates() {
        let policy = Policy::from_str(MEMORY_POLICY).unwrap();
        let (_, decision) = evaluate(make_memory_proposal("forget", None), &policy, None);
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn structured_missing_action_rejected() {
        let policy = Policy::from_str(MEMORY_POLICY).unwrap();
        let proposal = ToolInvocation::new("memory", "execute", json!({"path": "preferences/x"}));
        let (_, decision) = evaluate(proposal, &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn structured_unmatched_action_rejected() {
        let policy = Policy::from_str(MEMORY_POLICY).unwrap();
        let (_, decision) = evaluate(make_memory_proposal("inject_persona", None), &policy, None);
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn bash_still_works_with_default_match_source() {
        // Bash uses default match_source = "command"; should be unaffected.
        let policy = Policy::from_str(MEMORY_POLICY).unwrap();
        // bash tool is not in this policy → rejected
        let (_, d) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        assert!(matches!(d, Decision::Reject));
    }

    // --- Budget enforcement tests (M12) ---

    const BUDGET_POLICY: &str = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]

[budget]
session_limit_usd = 1.00
daily_limit_usd = 10.00
on_exceeded = "escalate"
"#;

    #[test]
    fn budget_under_limit_passes_through() {
        let policy = Policy::from_str(BUDGET_POLICY).unwrap();
        let ctx = BudgetContext {
            session_cost_usd: 0.50,
            daily_cost_usd: 5.0,
        };
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, Some(&ctx));
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe) when under budget"),
        }
    }

    #[test]
    fn budget_session_exceeded_escalates() {
        let policy = Policy::from_str(BUDGET_POLICY).unwrap();
        let ctx = BudgetContext {
            session_cost_usd: 1.50,
            daily_cost_usd: 5.0,
        };
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, Some(&ctx));
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn budget_daily_exceeded_escalates() {
        let policy = Policy::from_str(BUDGET_POLICY).unwrap();
        let ctx = BudgetContext {
            session_cost_usd: 0.50,
            daily_cost_usd: 10.0,
        };
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, Some(&ctx));
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn budget_exceeded_reject_mode() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]

[budget]
session_limit_usd = 1.00
on_exceeded = "reject"
"#;
        let policy = Policy::from_str(toml).unwrap();
        let ctx = BudgetContext {
            session_cost_usd: 1.50,
            daily_cost_usd: 0.0,
        };
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, Some(&ctx));
        assert!(matches!(decision, Decision::Reject));
    }

    #[test]
    fn budget_exactly_at_limit_escalates() {
        let policy = Policy::from_str(BUDGET_POLICY).unwrap();
        let ctx = BudgetContext {
            session_cost_usd: 1.00,
            daily_cost_usd: 5.0,
        };
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, Some(&ctx));
        // >= limit → escalates
        assert!(matches!(
            decision,
            Decision::Escalate { tier: Tier::Commit }
        ));
    }

    #[test]
    fn no_budget_config_passes_through() {
        // Default policy has no [budget] section.
        let policy = Policy::from_str(DEFAULT_POLICY).unwrap();
        let ctx = BudgetContext {
            session_cost_usd: 999.0,
            daily_cost_usd: 999.0,
        };
        // Budget context present but no budget configured → normal evaluation.
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, Some(&ctx));
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe) with no budget configured"),
        }
    }

    #[test]
    fn budget_none_context_passes_through() {
        let policy = Policy::from_str(BUDGET_POLICY).unwrap();
        // Budget configured but no context provided → normal evaluation.
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, None);
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow(Observe) with None budget context"),
        }
    }

    #[test]
    fn budget_only_session_limit() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]

[budget]
session_limit_usd = 1.00
"#;
        let policy = Policy::from_str(toml).unwrap();
        // Daily cost is irrelevant when daily_limit_usd is not set.
        let ctx = BudgetContext {
            session_cost_usd: 0.50,
            daily_cost_usd: 9999.0,
        };
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, Some(&ctx));
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow — only session limit configured, not exceeded"),
        }
    }

    #[test]
    fn budget_only_daily_limit() {
        let toml = r#"
[tools.bash]
enabled = true

[tools.bash.actions.read]
tier = "observe"
patterns = ["^ls "]

[budget]
daily_limit_usd = 10.00
"#;
        let policy = Policy::from_str(toml).unwrap();
        // Session cost is irrelevant when session_limit_usd is not set.
        let ctx = BudgetContext {
            session_cost_usd: 9999.0,
            daily_cost_usd: 5.0,
        };
        let (_, decision) = evaluate(make_proposal("bash", "ls /tmp"), &policy, Some(&ctx));
        match decision {
            Decision::Allow(token) => assert_eq!(token.tier, Tier::Observe),
            _ => panic!("expected Allow — only daily limit configured, not exceeded"),
        }
    }
}
