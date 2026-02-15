/// Build the system prompt for the agent.
///
/// Minimal prompt — no safety guardrails (enforcement layer handles that).
pub fn build_system_prompt(cwd: &str) -> String {
    format!(
        "You are a coding assistant with access to a bash tool for running commands.\n\
         \n\
         Current working directory: {cwd}\n\
         \n\
         Use the bash tool to run commands when the user asks you to interact with the system.\n\
         Explain what you're doing and share relevant output with the user.\n\
         If a command fails or is rejected, inform the user and suggest alternatives."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_cwd() {
        let prompt = build_system_prompt("/home/user/project");
        assert!(prompt.contains("/home/user/project"));
    }
}
