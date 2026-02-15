pub mod capability;
pub mod policy;
pub mod tier;

use capability::CapabilityToken;
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
/// Stub: denies by default. Milestone 1 adds policy evaluation.
pub fn evaluate(proposal: ToolInvocation<Proposed>) -> (ToolInvocation<Evaluated>, Decision) {
    (proposal.transition(), Decision::Reject)
}
