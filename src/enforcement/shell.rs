/// Parse a compound bash command into individual simple commands.
///
/// Returns `None` if the syntax cannot be safely parsed (deny-by-default).
/// Each returned string is a single command (trimmed, non-empty).
///
/// Splits on unquoted `;`, `&&`, `||`, `|`, and `\n`.
/// Recursively extracts commands from `$(...)` and `` `...` `` substitutions.
/// Rejects null bytes and unbalanced quotes (deny-by-default).
///
/// Redirections (`>`, `<`) are NOT command boundaries — they are part of the
/// command they belong to. Addressable via policy constraints.
pub(super) fn parse_commands(input: &str) -> Option<Vec<&str>> {
    // Null bytes anywhere → unparseable.
    if input.bytes().any(|b| b == 0) {
        return None;
    }

    let mut commands = Vec::new();
    collect_commands(input, &mut commands)?;
    Some(commands)
}

/// State machine for tracking quoting context.
#[derive(Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Walk `input`, splitting on unquoted command boundaries and extracting
/// commands from substitutions. Appends trimmed, non-empty slices to `out`.
/// Returns `None` on unparseable syntax.
fn collect_commands<'a>(input: &'a str, out: &mut Vec<&'a str>) -> Option<()> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut quote = Quote::None;
    let mut cmd_start: usize = 0;
    let mut i: usize = 0;

    // Track substitution regions to extract inner commands.
    let mut substitutions: Vec<(usize, usize)> = Vec::new();

    while i < len {
        let b = bytes[i];

        match quote {
            Quote::Single => {
                if b == b'\'' {
                    quote = Quote::None;
                }
                i += 1;
            }
            Quote::Double => {
                if b == b'\\' && i + 1 < len {
                    i += 2; // skip escaped char inside double quotes
                } else if b == b'"' {
                    quote = Quote::None;
                    i += 1;
                } else if b == b'$' && i + 1 < len && bytes[i + 1] == b'(' {
                    // Command substitution inside double quotes.
                    let inner_start = i + 2;
                    let close = find_matching_paren(bytes, inner_start)?;
                    substitutions.push((inner_start, close));
                    i = close + 1;
                } else if b == b'`' {
                    // Backtick substitution inside double quotes.
                    let inner_start = i + 1;
                    let close = find_backtick_close(bytes, inner_start)?;
                    substitutions.push((inner_start, close));
                    i = close + 1;
                } else {
                    i += 1;
                }
            }
            Quote::None => {
                if b == b'\\' && i + 1 < len {
                    i += 2; // skip escaped char
                } else if b == b'\'' {
                    quote = Quote::Single;
                    i += 1;
                } else if b == b'"' {
                    quote = Quote::Double;
                    i += 1;
                } else if b == b'$' && i + 1 < len && bytes[i + 1] == b'(' {
                    // Command substitution: $(...)
                    let inner_start = i + 2;
                    let close = find_matching_paren(bytes, inner_start)?;
                    substitutions.push((inner_start, close));
                    i = close + 1;
                } else if b == b'`' {
                    // Backtick substitution: `...`
                    let inner_start = i + 1;
                    let close = find_backtick_close(bytes, inner_start)?;
                    substitutions.push((inner_start, close));
                    i = close + 1;
                } else if b == b'<' && i + 1 < len && bytes[i + 1] == b'(' {
                    // Process substitution <(...) → unparseable.
                    return None;
                } else if b == b'<' && i + 1 < len && bytes[i + 1] == b'<' {
                    // Here-document <<... → unparseable.
                    return None;
                } else if b == b';' || b == b'\n' {
                    // Simple command boundary.
                    push_trimmed(input, cmd_start, i, out);
                    cmd_start = i + 1;
                    i += 1;
                } else if b == b'&' && i + 1 < len && bytes[i + 1] == b'&' {
                    // && boundary.
                    push_trimmed(input, cmd_start, i, out);
                    cmd_start = i + 2;
                    i += 2;
                } else if b == b'|' && i + 1 < len && bytes[i + 1] == b'|' {
                    // || boundary.
                    push_trimmed(input, cmd_start, i, out);
                    cmd_start = i + 2;
                    i += 2;
                } else if b == b'|' {
                    // Pipe boundary.
                    push_trimmed(input, cmd_start, i, out);
                    cmd_start = i + 1;
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }

    // Unbalanced quotes → unparseable.
    if quote != Quote::None {
        return None;
    }

    // Push final segment.
    push_trimmed(input, cmd_start, len, out);

    // Recursively extract commands from substitutions.
    for (start, end) in substitutions {
        let inner = &input[start..end];
        collect_commands(inner, out)?;
    }

    Some(())
}

/// Find the matching `)` for a `$(` starting at `start` (the byte after `(`).
/// Handles nested `$(...)` and quoting within the substitution.
/// Returns the index of the closing `)`.
fn find_matching_paren(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth: usize = 1;
    let mut i = start;
    let mut quote = Quote::None;

    while i < bytes.len() {
        let b = bytes[i];

        match quote {
            Quote::Single => {
                if b == b'\'' {
                    quote = Quote::None;
                }
                i += 1;
            }
            Quote::Double => {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else if b == b'"' {
                    quote = Quote::None;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            Quote::None => {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else if b == b'\'' {
                    quote = Quote::Single;
                    i += 1;
                } else if b == b'"' {
                    quote = Quote::Double;
                    i += 1;
                } else if b == b'(' {
                    depth += 1;
                    i += 1;
                } else if b == b')' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }

    None // unbalanced
}

/// Find the closing backtick for a `` ` `` substitution.
/// Backtick substitutions cannot nest — inner backticks must be escaped.
fn find_backtick_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else if bytes[i] == b'`' {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None // unbalanced
}

/// Push a trimmed, non-empty slice of `input[start..end]` into `out`.
fn push_trimmed<'a>(input: &'a str, start: usize, end: usize, out: &mut Vec<&'a str>) {
    let slice = &input[start..end];
    let trimmed = slice.trim();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command() {
        assert_eq!(parse_commands("ls /tmp"), Some(vec!["ls /tmp"]));
    }

    #[test]
    fn simple_pwd() {
        assert_eq!(parse_commands("pwd"), Some(vec!["pwd"]));
    }

    #[test]
    fn simple_echo() {
        assert_eq!(
            parse_commands("echo hello world"),
            Some(vec!["echo hello world"])
        );
    }

    #[test]
    fn pipe_splitting() {
        assert_eq!(parse_commands("ls | head"), Some(vec!["ls", "head"]));
    }

    #[test]
    fn semicolons() {
        assert_eq!(parse_commands("ls; pwd"), Some(vec!["ls", "pwd"]));
    }

    #[test]
    fn logical_and() {
        assert_eq!(parse_commands("ls && pwd"), Some(vec!["ls", "pwd"]));
    }

    #[test]
    fn logical_or() {
        assert_eq!(parse_commands("ls || pwd"), Some(vec!["ls", "pwd"]));
    }

    #[test]
    fn mixed_operators() {
        assert_eq!(
            parse_commands("ls /tmp | head; echo done"),
            Some(vec!["ls /tmp", "head", "echo done"])
        );
    }

    #[test]
    fn single_quoted_semicolon() {
        assert_eq!(
            parse_commands("echo 'hello; world'"),
            Some(vec!["echo 'hello; world'"])
        );
    }

    #[test]
    fn double_quoted_pipe() {
        assert_eq!(
            parse_commands("echo \"hello | world\""),
            Some(vec!["echo \"hello | world\""])
        );
    }

    #[test]
    fn command_substitution_dollar_paren() {
        let result = parse_commands("echo $(pwd)").unwrap();
        assert!(result.contains(&"echo $(pwd)"));
        assert!(result.contains(&"pwd"));
    }

    #[test]
    fn command_substitution_backtick() {
        let result = parse_commands("echo `pwd`").unwrap();
        assert!(result.contains(&"echo `pwd`"));
        assert!(result.contains(&"pwd"));
    }

    #[test]
    fn nested_substitution() {
        let result = parse_commands("echo $(ls $(pwd))").unwrap();
        assert!(result.contains(&"echo $(ls $(pwd))"));
        assert!(result.contains(&"ls $(pwd)"));
        assert!(result.contains(&"pwd"));
    }

    #[test]
    fn null_byte_rejected() {
        assert_eq!(parse_commands("ls\0rm"), None);
    }

    #[test]
    fn unbalanced_single_quote() {
        assert_eq!(parse_commands("echo 'hello"), None);
    }

    #[test]
    fn unbalanced_double_quote() {
        assert_eq!(parse_commands("echo \"hello"), None);
    }

    #[test]
    fn empty_string() {
        assert_eq!(parse_commands(""), Some(vec![]));
    }

    #[test]
    fn whitespace_only() {
        assert_eq!(parse_commands("   "), Some(vec![]));
    }

    #[test]
    fn escaped_characters() {
        assert_eq!(
            parse_commands("echo hello\\ world"),
            Some(vec!["echo hello\\ world"])
        );
    }

    #[test]
    fn newline_as_separator() {
        assert_eq!(parse_commands("ls\npwd"), Some(vec!["ls", "pwd"]));
    }

    #[test]
    fn here_document_rejected() {
        assert_eq!(parse_commands("cat <<EOF\nhello\nEOF"), None);
    }

    #[test]
    fn process_substitution_rejected() {
        assert_eq!(parse_commands("diff <(ls /a) <(ls /b)"), None);
    }

    #[test]
    fn substitution_inside_double_quotes() {
        let result = parse_commands("echo \"$(pwd)\"").unwrap();
        assert!(result.contains(&"echo \"$(pwd)\""));
        assert!(result.contains(&"pwd"));
    }

    #[test]
    fn backtick_inside_double_quotes() {
        let result = parse_commands("echo \"`pwd`\"").unwrap();
        assert!(result.contains(&"echo \"`pwd`\""));
        assert!(result.contains(&"pwd"));
    }

    #[test]
    fn unbalanced_dollar_paren() {
        assert_eq!(parse_commands("echo $(pwd"), None);
    }

    #[test]
    fn unbalanced_backtick() {
        assert_eq!(parse_commands("echo `pwd"), None);
    }
}
