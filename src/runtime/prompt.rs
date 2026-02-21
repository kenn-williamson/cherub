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

/// Format messages as readable conversation text for summarization/extraction prompts.
///
/// Pure function: no I/O, no side effects, unit-testable.
/// Used by compaction to present conversation history to the LLM for summarization.
pub fn serialize_messages_for_prompt(messages: &[crate::providers::Message]) -> String {
    use crate::providers::{ContentBlock, Message, UserContent};

    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::User { content } => {
                out.push_str("User: ");
                for c in content {
                    match c {
                        UserContent::Text(text) => out.push_str(text),
                        UserContent::Image { .. } => out.push_str("[image]"),
                    }
                }
                out.push('\n');
            }
            Message::Assistant { content, .. } => {
                out.push_str("Assistant: ");
                for block in content {
                    match block {
                        ContentBlock::Text { text } => out.push_str(text),
                        ContentBlock::ToolUse { name, input, .. } => {
                            out.push_str(&format!("[tool_use: {name}({input})]"));
                        }
                    }
                }
                out.push('\n');
            }
            Message::ToolResult {
                content, is_error, ..
            } => {
                if *is_error {
                    out.push_str(&format!("Tool Error: {content}\n"));
                } else {
                    out.push_str(&format!("Tool Result: {content}\n"));
                }
            }
        }
    }
    out
}

/// Build the system prompt for the agent.
///
/// Minimal prompt — no safety guardrails (enforcement layer handles that).
/// Sections are appended based on which features are compiled in.
pub fn build_system_prompt(cwd: &str) -> String {
    // Build inside a block so `p` is always mut regardless of which features are
    // enabled — avoids "unused_mut" warnings when neither memory nor credentials
    // are compiled in.
    {
        #[allow(unused_mut)] // mut used by push_str when memory/credentials features are active
        let mut p = format!(
            "You are a coding assistant with access to a bash tool for running commands.\n\
             \n\
             Current working directory: {cwd}\n\
             \n\
             Use the bash tool to run commands when the user asks you to interact with the system.\n\
             Explain what you're doing and share relevant output with the user.\n\
             If a command fails or is rejected, inform the user and suggest alternatives."
        );

        #[cfg(feature = "memory")]
        p.push_str(
            "\n\n\
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
            will return 'action not permitted'.",
        );

        #[cfg(feature = "credentials")]
        p.push_str(
            "\n\n\
            ## HTTP Tool and Credentials\n\
            \n\
            You have access to an HTTP tool for calling external APIs.\n\
            \n\
            **Usage**: specify `action` (get/post/put/patch/delete), `url`, optional `headers`,\n\
            optional `body`, and optional `credential` (name of a stored credential).\n\
            \n\
            **Credentials**: reference stored credentials by name only — you never see their\n\
            values. The runtime injects the credential into the HTTP request at execution time.\n\
            Specify the credential name in the `credential` field; the value is never revealed.\n\
            \n\
            Policy enforcement controls which hosts and methods are permitted.\n\
            Requests to hosts not in the policy will be rejected.",
        );

        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_cwd() {
        let prompt = build_system_prompt("/home/user/project");
        assert!(prompt.contains("/home/user/project"));
    }

    #[test]
    fn serialize_messages_user_and_assistant() {
        use crate::providers::{ContentBlock, Message, StopReason};
        let messages = vec![
            Message::user_text("Hello"),
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "Hi there!".to_owned(),
                }],
                stop_reason: StopReason::EndTurn,
            },
        ];
        let result = serialize_messages_for_prompt(&messages);
        assert!(result.contains("User: Hello"));
        assert!(result.contains("Assistant: Hi there!"));
    }

    #[test]
    fn serialize_messages_tool_use_and_result() {
        use crate::providers::{ContentBlock, Message, StopReason};
        let messages = vec![
            Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                stop_reason: StopReason::ToolUse,
            },
            Message::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: "file.txt".to_owned(),
                is_error: false,
            },
        ];
        let result = serialize_messages_for_prompt(&messages);
        assert!(result.contains("[tool_use: bash("));
        assert!(result.contains("Tool Result: file.txt"));
    }

    #[test]
    fn serialize_messages_tool_error() {
        use crate::providers::Message;
        let messages = vec![Message::ToolResult {
            tool_use_id: "t1".to_owned(),
            content: "action not permitted".to_owned(),
            is_error: true,
        }];
        let result = serialize_messages_for_prompt(&messages);
        assert!(result.contains("Tool Error: action not permitted"));
    }

    #[test]
    fn serialize_messages_empty() {
        assert_eq!(serialize_messages_for_prompt(&[]), "");
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
