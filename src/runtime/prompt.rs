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
}
