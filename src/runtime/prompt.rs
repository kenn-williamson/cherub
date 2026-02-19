/// Format the memory injection section appended to the system prompt before each turn.
///
/// Splits memories into **Verified** (Explicit/Confirmed) and **Inferred** subsections.
/// Returns an empty string when `memories` is empty — callers skip appending in that case.
/// This is a pure function: no I/O, no side effects, unit-testable.
#[cfg(feature = "memory")]
pub fn format_memory_injection(memories: &[crate::storage::Memory]) -> String {
    if memories.is_empty() {
        return String::new();
    }

    let verified: Vec<&crate::storage::Memory> = memories
        .iter()
        .filter(|m| {
            matches!(
                m.source_type,
                crate::storage::SourceType::Explicit | crate::storage::SourceType::Confirmed
            )
        })
        .collect();

    let inferred: Vec<&crate::storage::Memory> = memories
        .iter()
        .filter(|m| matches!(m.source_type, crate::storage::SourceType::Inferred))
        .collect();

    let mut out = "\n\n## Relevant memories\n".to_owned();

    if !verified.is_empty() {
        out.push_str("\n### Verified\n");
        for m in &verified {
            out.push_str(&format!(
                "- {} [{}, {}]\n",
                m.content,
                m.source_type.as_str(),
                m.created_at.format("%Y-%m-%d"),
            ));
        }
    }

    if !inferred.is_empty() {
        out.push_str("\n### Inferred (lower confidence)\n");
        for m in &inferred {
            out.push_str(&format!(
                "- {} [inferred, confidence: {:.2}]\n",
                m.content, m.confidence,
            ));
        }
    }

    out
}

/// Build the system prompt for the agent.
///
/// Minimal prompt — no safety guardrails (enforcement layer handles that).
pub fn build_system_prompt(cwd: &str) -> String {
    let base = format!(
        "You are a coding assistant with access to a bash tool for running commands.\n\
         \n\
         Current working directory: {cwd}\n\
         \n\
         Use the bash tool to run commands when the user asks you to interact with the system.\n\
         Explain what you're doing and share relevant output with the user.\n\
         If a command fails or is rejected, inform the user and suggest alternatives."
    );

    #[cfg(feature = "memory")]
    {
        let memory_section = "\n\n\
            ## Memory\n\
            \n\
            You have access to a memory tool for storing information across sessions.\n\
            \n\
            **Operations**: store, recall, search, update, forget\n\
            **Scopes**: user (per-user, default), agent (shared), working (ephemeral context)\n\
            **Categories**: preference, fact, instruction, identity, observation\n\
            **Paths**: hierarchical, e.g. 'preferences/food', 'identity/values'\n\
            \n\
            Use memory to:\n\
            - Store user preferences (scope: user, category: preference, path: preferences/...)\n\
            - Record facts about the user or project (category: fact)\n\
            - Track working context across turns (scope: working, category: observation)\n\
            \n\
            Do NOT store sensitive credentials, secrets, or private data.\n\
            Do NOT modify agent-scope or identity memories without explicit user instruction.\n\
            Policy enforcement controls which operations are permitted — rejected operations\n\
            will return 'action not permitted'.";
        format!("{base}{memory_section}")
    }

    #[cfg(not(feature = "memory"))]
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_cwd() {
        let prompt = build_system_prompt("/home/user/project");
        assert!(prompt.contains("/home/user/project"));
    }

    #[cfg(feature = "memory")]
    #[test]
    fn prompt_contains_memory_instructions() {
        let prompt = build_system_prompt("/tmp");
        assert!(prompt.contains("memory tool"));
        assert!(prompt.contains("preferences"));
    }

    #[cfg(feature = "memory")]
    mod injection {
        use chrono::Utc;
        use uuid::{NoContext, Timestamp, Uuid};

        use crate::storage::{Memory, MemoryCategory, MemoryScope, SourceType};

        fn new_uuid() -> Uuid {
            Uuid::new_v7(Timestamp::now(NoContext))
        }

        fn make_memory(content: &str, source_type: SourceType, confidence: f32) -> Memory {
            Memory {
                id: new_uuid(),
                user_id: "test".to_owned(),
                scope: MemoryScope::User,
                category: MemoryCategory::Preference,
                path: "test/path".to_owned(),
                content: content.to_owned(),
                structured: None,
                source_session_id: None,
                source_turn_number: None,
                source_type,
                confidence,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                last_referenced_at: None,
                superseded_by: None,
            }
        }

        #[test]
        fn empty_memories_returns_empty_string() {
            assert_eq!(super::super::format_memory_injection(&[]), "");
        }

        #[test]
        fn verified_only() {
            let memories = vec![
                make_memory("User is allergic to peanuts", SourceType::Explicit, 1.0),
                make_memory("Grocery budget: $200/week", SourceType::Confirmed, 1.0),
            ];
            let result = super::super::format_memory_injection(&memories);
            assert!(result.contains("## Relevant memories"));
            assert!(result.contains("### Verified"));
            assert!(result.contains("User is allergic to peanuts"));
            assert!(result.contains("Grocery budget: $200/week"));
            assert!(!result.contains("### Inferred"));
        }

        #[test]
        fn inferred_only() {
            let memories = vec![make_memory(
                "User usually orders groceries on Sundays",
                SourceType::Inferred,
                0.60,
            )];
            let result = super::super::format_memory_injection(&memories);
            assert!(result.contains("## Relevant memories"));
            assert!(result.contains("### Inferred (lower confidence)"));
            assert!(result.contains("User usually orders groceries on Sundays"));
            assert!(result.contains("confidence: 0.60"));
            assert!(!result.contains("### Verified"));
        }

        #[test]
        fn mixed_verified_and_inferred() {
            let memories = vec![
                make_memory("Explicit fact", SourceType::Explicit, 1.0),
                make_memory("Inferred behavior", SourceType::Inferred, 0.75),
            ];
            let result = super::super::format_memory_injection(&memories);
            assert!(result.contains("### Verified"));
            assert!(result.contains("### Inferred (lower confidence)"));
            assert!(result.contains("Explicit fact"));
            assert!(result.contains("Inferred behavior"));
            assert!(result.contains("confidence: 0.75"));
        }

        #[test]
        fn injection_appended_to_system_prompt() {
            let base = "You are a helpful assistant.";
            let memories = vec![make_memory(
                "User prefers dark mode",
                SourceType::Explicit,
                1.0,
            )];
            let injection = super::super::format_memory_injection(&memories);
            let effective = format!("{base}{injection}");
            assert!(effective.starts_with("You are a helpful assistant."));
            assert!(effective.contains("## Relevant memories"));
            assert!(effective.contains("User prefers dark mode"));
        }
    }
}
