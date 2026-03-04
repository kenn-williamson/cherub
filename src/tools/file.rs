//! File tool: structured file operations for the agent.
//!
//! Provides read, edit, glob, and grep actions with workspace containment.
//! All paths are relative to the workspace root. Absolute paths and directory
//! traversal are rejected. Symlinks are resolved and re-checked.
//!
//! Uses `MatchSource::Structured` for enforcement — the action string is
//! `"{action}:{path}"` or `"{action}"`, matching the memory tool pattern.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use regex::RegexBuilder;
use tracing::{info_span, warn};

use crate::enforcement::capability::CapabilityToken;
use crate::error::CherubError;
use crate::tools::ToolResult;
use crate::tools::path::{is_binary_content, resolve_workspace_path};

/// Maximum lines returned by `read` before truncation.
const READ_MAX_LINES: usize = 2_000;
/// Maximum bytes returned by `read` before truncation.
const READ_MAX_BYTES: usize = 256 * 1024;
/// Maximum characters per match line in `grep` output.
const GREP_MAX_LINE_CHARS: usize = 500;
/// Maximum matches returned by `grep` before truncation.
const GREP_MAX_MATCHES: usize = 100;
/// Maximum bytes of total `grep` output.
const GREP_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Maximum entries returned by `glob` before truncation.
const GLOB_MAX_ENTRIES: usize = 1_000;

/// UTF-8 BOM (byte order mark).
const UTF8_BOM: &str = "\u{FEFF}";

pub struct FileTool {
    workspace_root: PathBuf,
}

impl FileTool {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }

    pub async fn execute(
        &self,
        params: &serde_json::Value,
        _token: CapabilityToken,
    ) -> Result<ToolResult, CherubError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CherubError::InvalidInvocation("file tool requires 'action'".to_owned())
            })?;

        match action {
            "read" => self.op_read(params),
            "edit" => self.op_edit(params),
            "glob" => self.op_glob(params),
            "grep" => self.op_grep(params),
            other => Err(CherubError::InvalidInvocation(format!(
                "unknown file action: {other}"
            ))),
        }
    }

    fn op_read(&self, params: &serde_json::Value) -> Result<ToolResult, CherubError> {
        let path_str = require_str(params, "path", "read")?;
        let _span = info_span!("file_read", path = %path_str);

        let resolved = resolve_workspace_path(&self.workspace_root, path_str)?;

        let raw_bytes = fs::read(&resolved)
            .map_err(|e| CherubError::ToolExecution(format!("cannot read '{path_str}': {e}")))?;

        if is_binary_content(&raw_bytes) {
            return Err(CherubError::ToolExecution(format!(
                "'{path_str}' appears to be a binary file"
            )));
        }

        let content = String::from_utf8_lossy(&raw_bytes);
        // Strip UTF-8 BOM — LLMs don't include it in edit old_string.
        let content = content.strip_prefix(UTF8_BOM).unwrap_or(&content);

        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n.max(1) as usize)
            .unwrap_or(1);

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        let all_lines: Vec<&str> = content.lines().collect();
        let total_lines = all_lines.len();

        // offset is 1-indexed.
        let start_idx = (offset - 1).min(total_lines);
        let available = &all_lines[start_idx..];

        let effective_limit = limit.unwrap_or(READ_MAX_LINES).min(READ_MAX_LINES);
        let mut output = String::new();

        for (line_count, (i, line)) in available.iter().enumerate().enumerate() {
            if line_count >= effective_limit || output.len() >= READ_MAX_BYTES {
                let shown_end = start_idx + i;
                let truncation_reason = if output.len() >= READ_MAX_BYTES {
                    "byte limit"
                } else {
                    "line limit"
                };
                output.push_str(&format!(
                    "\n[Showing lines {}-{} of {} ({truncation_reason}). Use offset={} to continue.]",
                    offset,
                    shown_end,
                    total_lines,
                    shown_end + 1,
                ));
                break;
            }
            let line_num = start_idx + i + 1;
            output.push_str(&format!("{line_num:>6}\u{2502}{line}\n"));
        }

        if output.is_empty() && total_lines > 0 {
            output = format!("[No lines in range. File has {total_lines} lines.]");
        } else if total_lines == 0 {
            output = "[Empty file]".to_owned();
        }

        Ok(ToolResult { output })
    }

    fn op_edit(&self, params: &serde_json::Value) -> Result<ToolResult, CherubError> {
        let path_str = require_str(params, "path", "edit")?;
        let old_string = require_str(params, "old_string", "edit")?;
        let new_string = require_str(params, "new_string", "edit")?;
        let replace_all = params
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let _span = info_span!("file_edit", path = %path_str, replace_all);

        if old_string == new_string {
            return Err(CherubError::ToolExecution(
                "old_string and new_string are identical — no-op edit rejected".to_owned(),
            ));
        }

        let resolved = resolve_workspace_path(&self.workspace_root, path_str)?;

        let raw_bytes = fs::read(&resolved)
            .map_err(|e| CherubError::ToolExecution(format!("cannot read '{path_str}': {e}")))?;

        if is_binary_content(&raw_bytes) {
            return Err(CherubError::ToolExecution(format!(
                "cannot edit binary file '{path_str}'"
            )));
        }

        let raw_content = String::from_utf8_lossy(&raw_bytes).into_owned();

        // Detect BOM and line endings.
        let has_bom = raw_content.starts_with(UTF8_BOM);
        let content_no_bom = if has_bom {
            &raw_content[UTF8_BOM.len()..]
        } else {
            &raw_content
        };
        let has_crlf = content_no_bom.contains("\r\n");

        // Normalize to LF for matching.
        let normalized = if has_crlf {
            content_no_bom.replace("\r\n", "\n")
        } else {
            content_no_bom.to_owned()
        };

        // Normalize old_string line endings to LF too.
        let old_normalized = old_string.replace("\r\n", "\n");

        // Try exact match first.
        let match_count = normalized.matches(&old_normalized).count();

        let (result_content, applied) = if match_count == 0 {
            // Try fuzzy match: normalize smart quotes/dashes/special spaces.
            let fuzzy_content = normalize_unicode(&normalized);
            let fuzzy_old = normalize_unicode(&old_normalized);
            let fuzzy_count = fuzzy_content.matches(&fuzzy_old).count();

            if fuzzy_count == 0 {
                return Err(CherubError::ToolExecution(
                    "old_string not found in file".to_owned(),
                ));
            }

            if fuzzy_count > 1 && !replace_all {
                return Err(CherubError::ToolExecution(format!(
                    "old_string found {fuzzy_count} times (after Unicode normalization); \
                     provide more context to make it unique, or set replace_all=true"
                )));
            }

            // Apply on the fuzzy-normalized content, then we can't map back easily.
            // Instead, do a character-position approach on the original.
            // For simplicity with fuzzy matching, apply on the normalized and work from there.
            let new_normalized = new_string.replace("\r\n", "\n");
            if replace_all {
                (
                    fuzzy_content.replace(&fuzzy_old, &new_normalized),
                    fuzzy_count,
                )
            } else {
                (fuzzy_content.replacen(&fuzzy_old, &new_normalized, 1), 1)
            }
        } else if match_count > 1 && !replace_all {
            return Err(CherubError::ToolExecution(format!(
                "old_string found {match_count} times; provide more context to make it unique, \
                 or set replace_all=true"
            )));
        } else {
            let new_normalized = new_string.replace("\r\n", "\n");
            if replace_all {
                (
                    normalized.replace(&old_normalized, &new_normalized),
                    match_count,
                )
            } else {
                (normalized.replacen(&old_normalized, &new_normalized, 1), 1)
            }
        };

        // Restore CRLF if original used it.
        let final_content = if has_crlf {
            result_content.replace('\n', "\r\n")
        } else {
            result_content
        };

        // Restore BOM if original had it.
        let to_write = if has_bom {
            format!("{UTF8_BOM}{final_content}")
        } else {
            final_content
        };

        fs::write(&resolved, to_write.as_bytes())
            .map_err(|e| CherubError::ToolExecution(format!("cannot write '{path_str}': {e}")))?;

        let msg = if applied == 1 {
            format!("edited '{path_str}' (1 replacement)")
        } else {
            format!("edited '{path_str}' ({applied} replacements)")
        };
        Ok(ToolResult { output: msg })
    }

    fn op_glob(&self, params: &serde_json::Value) -> Result<ToolResult, CherubError> {
        let pattern = require_str(params, "pattern", "glob")?;
        let base_dir = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let _span = info_span!("file_glob", pattern = %pattern, base = %base_dir);

        // Resolve the base directory.
        let resolved_base = if base_dir == "." {
            self.workspace_root.clone()
        } else {
            resolve_workspace_path(&self.workspace_root, base_dir)?
        };

        // Build the full glob pattern.
        let full_pattern = resolved_base.join(pattern);
        let full_pattern_str = full_pattern.to_string_lossy();

        let entries: Result<Vec<_>, _> = glob::glob(&full_pattern_str)
            .map_err(|e| CherubError::ToolExecution(format!("invalid glob pattern: {e}")))?
            .collect();

        let entries =
            entries.map_err(|e| CherubError::ToolExecution(format!("glob error: {e}")))?;

        let canonical_root = self.workspace_root.canonicalize().map_err(|e| {
            CherubError::ToolExecution(format!("failed to resolve workspace root: {e}"))
        })?;

        // Filter: must be inside workspace, resolve symlinks.
        let mut valid_entries: Vec<(PathBuf, SystemTime)> = Vec::new();
        for entry in &entries {
            let canonical = match entry.canonicalize() {
                Ok(c) => c,
                Err(_) => continue, // skip broken symlinks
            };
            if !canonical.starts_with(&canonical_root) {
                warn!(path = %entry.display(), "glob result escapes workspace, skipping");
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            // Store relative path.
            if let Ok(rel) = canonical.strip_prefix(&canonical_root) {
                valid_entries.push((rel.to_path_buf(), mtime));
            }
        }

        // Sort by mtime, newest first.
        valid_entries.sort_by(|a, b| b.1.cmp(&a.1));

        let total = valid_entries.len();
        let truncated = total > GLOB_MAX_ENTRIES;
        let shown = total.min(GLOB_MAX_ENTRIES);

        let mut output: String = valid_entries
            .iter()
            .take(GLOB_MAX_ENTRIES)
            .map(|(p, _)| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        if truncated {
            output.push_str(&format!(
                "\n[Showing {shown} of {total} results. Narrow your pattern.]"
            ));
        }

        if output.is_empty() {
            output = "no matches".to_owned();
        }

        Ok(ToolResult { output })
    }

    fn op_grep(&self, params: &serde_json::Value) -> Result<ToolResult, CherubError> {
        let pattern = require_str(params, "pattern", "grep")?;
        let search_root = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let include = params.get("include").and_then(|v| v.as_str());
        let context_lines = params.get("context").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let _span = info_span!("file_grep", pattern = %pattern, root = %search_root);

        // Compile regex with safety limits per CLAUDE.md crate rules.
        let regex = RegexBuilder::new(pattern)
            .size_limit(1 << 20)
            .nest_limit(50)
            .unicode(false)
            .build()
            .map_err(|e| CherubError::ToolExecution(format!("invalid regex pattern: {e}")))?;

        // Compile include glob if provided.
        let include_pattern = include
            .map(glob::Pattern::new)
            .transpose()
            .map_err(|e| CherubError::ToolExecution(format!("invalid include pattern: {e}")))?;

        // Resolve search root.
        let resolved_root = if search_root == "." {
            self.workspace_root.clone()
        } else {
            resolve_workspace_path(&self.workspace_root, search_root)?
        };

        let canonical_root = self.workspace_root.canonicalize().map_err(|e| {
            CherubError::ToolExecution(format!("failed to resolve workspace root: {e}"))
        })?;

        let mut output = String::new();
        let mut match_count: usize = 0;
        let mut hit_limit = false;

        walk_dir_recursive(&resolved_root, &canonical_root, &mut |file_path| {
            if hit_limit {
                return;
            }

            // Apply include filter against the file name.
            if let Some(ref pat) = include_pattern {
                if let Some(name) = file_path.file_name().and_then(|n| n.to_str()) {
                    if !pat.matches(name) {
                        return;
                    }
                } else {
                    return;
                }
            }

            // Read file, skip binary.
            let bytes = match fs::read(file_path) {
                Ok(b) => b,
                Err(_) => return,
            };
            if is_binary_content(&bytes) {
                return;
            }
            let content = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(_) => return,
            };

            let lines: Vec<&str> = content.lines().collect();
            let rel_path = file_path
                .strip_prefix(&canonical_root)
                .unwrap_or(file_path)
                .display();

            for (i, line) in lines.iter().enumerate() {
                if regex.is_match(line) {
                    if match_count >= GREP_MAX_MATCHES || output.len() >= GREP_MAX_OUTPUT_BYTES {
                        hit_limit = true;
                        return;
                    }
                    match_count += 1;

                    // Print context before.
                    let ctx_start = i.saturating_sub(context_lines);
                    for (ci, ctx_src) in lines.iter().enumerate().take(i).skip(ctx_start) {
                        let ctx_line = truncate_line(ctx_src, GREP_MAX_LINE_CHARS);
                        output.push_str(&format!("{rel_path}:{}-{ctx_line}\n", ci + 1));
                    }

                    // Print match line.
                    let truncated = truncate_line(line, GREP_MAX_LINE_CHARS);
                    output.push_str(&format!("{rel_path}:{}:{truncated}\n", i + 1));

                    // Print context after.
                    let ctx_end = (i + context_lines + 1).min(lines.len());
                    for (ci, ctx_src) in lines.iter().enumerate().take(ctx_end).skip(i + 1) {
                        let ctx_line = truncate_line(ctx_src, GREP_MAX_LINE_CHARS);
                        output.push_str(&format!("{rel_path}:{}-{ctx_line}\n", ci + 1));
                    }
                }
            }
        });

        if hit_limit {
            output.push_str(&format!(
                "\n[{match_count} matches shown, limit reached. Narrow your search.]"
            ));
        }

        if output.is_empty() {
            output = "no matches".to_owned();
        }

        Ok(ToolResult { output })
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn require_str<'a>(
    params: &'a serde_json::Value,
    field: &str,
    action: &str,
) -> Result<&'a str, CherubError> {
    params
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| CherubError::InvalidInvocation(format!("{action} requires '{field}'")))
}

/// Normalize Unicode characters that LLMs commonly substitute.
///
/// Smart quotes → straight quotes, em/en dashes → hyphens,
/// non-breaking/thin/hair spaces → regular spaces.
fn normalize_unicode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            '\u{00A0}' | '\u{2007}' | '\u{2008}' | '\u{200A}' | '\u{202F}' | '\u{205F}' => ' ',
            other => other,
        })
        .collect()
}

/// Truncate a line to `max_chars` characters.
fn truncate_line(line: &str, max_chars: usize) -> &str {
    if line.len() <= max_chars {
        return line;
    }
    // Find a char boundary at or before max_chars.
    let mut end = max_chars;
    while !line.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    &line[..end]
}

/// Recursively walk a directory, calling `cb` for each file.
///
/// Skips `.git/` directories and entries that escape the canonical root.
fn walk_dir_recursive(dir: &Path, canonical_root: &Path, cb: &mut impl FnMut(&Path)) {
    let read_dir = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    let mut entries: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();

        // Skip .git directories.
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name == ".git"
        {
            continue;
        }

        // Containment check after resolving symlinks.
        let canonical = match path.canonicalize() {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !canonical.starts_with(canonical_root) {
            continue;
        }

        if canonical.is_dir() {
            walk_dir_recursive(&canonical, canonical_root, cb);
        } else if canonical.is_file() {
            cb(&canonical);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool(dir: &Path) -> FileTool {
        FileTool::new(dir.to_path_buf())
    }

    fn allow_token() -> CapabilityToken {
        use crate::enforcement::{self, policy::Policy};
        use crate::tools::{Proposed, ToolInvocation};
        use std::str::FromStr;

        let policy_str = r#"
[tools.file]
enabled = true
match_source = "structured"

[tools.file.actions.read_ops]
tier = "observe"
patterns = ["^read:", "^read$", "^glob:", "^glob$", "^grep:", "^grep$"]

[tools.file.actions.write_ops]
tier = "act"
patterns = ["^edit:", "^edit$"]
"#;
        let policy = Policy::from_str(policy_str).unwrap();
        let proposal = ToolInvocation::<Proposed>::new(
            "file",
            "execute",
            json!({"action": "read", "path": "test.txt"}),
        );
        let (_, decision) = enforcement::evaluate(proposal, &policy, None);
        match decision {
            enforcement::Decision::Allow(token) => token,
            _ => panic!("expected Allow"),
        }
    }

    // --- read tests ---

    #[tokio::test]
    async fn read_simple_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("hello.txt"),
            "line one\nline two\nline three\n",
        )
        .unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "read", "path": "hello.txt"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("1\u{2502}line one"));
        assert!(result.output.contains("2\u{2502}line two"));
        assert!(result.output.contains("3\u{2502}line three"));
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.path().join("big.txt"), &content).unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "read", "path": "big.txt", "offset": 50, "limit": 5}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("50\u{2502}line 50"));
        assert!(result.output.contains("54\u{2502}line 54"));
        assert!(!result.output.contains("55\u{2502}"));
    }

    #[tokio::test]
    async fn read_binary_file_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bin.dat"), b"hello\0world").unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(&json!({"action": "read", "path": "bin.dat"}), allow_token())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("binary file"));
    }

    #[tokio::test]
    async fn read_bom_stripped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bom.txt"), "\u{FEFF}hello bom\n").unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(&json!({"action": "read", "path": "bom.txt"}), allow_token())
            .await
            .unwrap();
        assert!(result.output.contains("hello bom"));
        // BOM should not appear in output.
        assert!(!result.output.contains('\u{FEFF}'));
    }

    #[tokio::test]
    async fn read_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("empty.txt"), "").unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "read", "path": "empty.txt"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("[Empty file]"));
    }

    #[tokio::test]
    async fn read_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=3000).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.path().join("huge.txt"), &content).unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "read", "path": "huge.txt"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("Use offset="));
        assert!(result.output.contains("of 3000"));
    }

    // --- edit tests ---

    #[tokio::test]
    async fn edit_unique_replacement() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("src.rs"), "fn hello() {}\nfn world() {}\n").unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "src.rs",
                    "old_string": "fn hello() {}",
                    "new_string": "fn hello() -> i32 { 42 }"
                }),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("1 replacement"));
        let content = fs::read_to_string(dir.path().join("src.rs")).unwrap();
        assert!(content.contains("fn hello() -> i32 { 42 }"));
        assert!(content.contains("fn world() {}"));
    }

    #[tokio::test]
    async fn edit_non_unique_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("dup.txt"), "foo\nfoo\n").unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "dup.txt",
                    "old_string": "foo",
                    "new_string": "bar"
                }),
                allow_token(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2 times"));
    }

    #[tokio::test]
    async fn edit_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("rep.txt"), "foo\nfoo\nfoo\n").unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "rep.txt",
                    "old_string": "foo",
                    "new_string": "bar",
                    "replace_all": true
                }),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("3 replacements"));
        let content = fs::read_to_string(dir.path().join("rep.txt")).unwrap();
        assert!(!content.contains("foo"));
        assert_eq!(content.matches("bar").count(), 3);
    }

    #[tokio::test]
    async fn edit_fuzzy_smart_quotes() {
        let dir = tempfile::tempdir().unwrap();
        // File has straight quotes.
        fs::write(dir.path().join("quote.txt"), "say \"hello\"\n").unwrap();
        let tool = make_tool(dir.path());
        // Agent sends smart quotes.
        let result = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "quote.txt",
                    "old_string": "say \u{201C}hello\u{201D}",
                    "new_string": "say \"world\""
                }),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("1 replacement"));
    }

    #[tokio::test]
    async fn edit_crlf_preservation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("win.txt"),
            "line one\r\nline two\r\nline three\r\n",
        )
        .unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "win.txt",
                    "old_string": "line two",
                    "new_string": "line TWO"
                }),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("1 replacement"));
        let content = fs::read_to_string(dir.path().join("win.txt")).unwrap();
        assert!(content.contains("\r\n"));
        assert!(content.contains("line TWO"));
    }

    #[tokio::test]
    async fn edit_noop_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("noop.txt"), "content").unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "noop.txt",
                    "old_string": "content",
                    "new_string": "content"
                }),
                allow_token(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no-op"));
    }

    #[tokio::test]
    async fn edit_binary_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bin.dat"), b"hello\0world").unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "bin.dat",
                    "old_string": "hello",
                    "new_string": "goodbye"
                }),
                allow_token(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("binary"));
    }

    #[tokio::test]
    async fn edit_not_found() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "hello world").unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "file.txt",
                    "old_string": "goodbye",
                    "new_string": "hello"
                }),
                allow_token(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // --- glob tests ---

    #[tokio::test]
    async fn glob_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "").unwrap();
        fs::write(dir.path().join("b.rs"), "").unwrap();
        fs::write(dir.path().join("c.txt"), "").unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(&json!({"action": "glob", "pattern": "*.rs"}), allow_token())
            .await
            .unwrap();
        assert!(result.output.contains("a.rs"));
        assert!(result.output.contains("b.rs"));
        assert!(!result.output.contains("c.txt"));
    }

    #[tokio::test]
    async fn glob_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "glob", "pattern": "*.nonexistent"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert_eq!(result.output, "no matches");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_symlink_escape_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let escape_target = tempfile::tempdir().unwrap();
        fs::write(escape_target.path().join("secret.rs"), "secret").unwrap();
        std::os::unix::fs::symlink(escape_target.path(), dir.path().join("escape")).unwrap();

        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "glob", "pattern": "escape/*.rs"}),
                allow_token(),
            )
            .await
            .unwrap();
        // The symlink-escaped file should not appear.
        assert!(!result.output.contains("secret.rs"));
    }

    // --- grep tests ---

    #[tokio::test]
    async fn grep_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "grep", "pattern": "println"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("println"));
        assert!(result.output.contains("code.rs:2:"));
    }

    #[tokio::test]
    async fn grep_with_context() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ctx.txt"), "aaa\nbbb\nccc\nddd\neee\n").unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "grep", "pattern": "ccc", "context": 1}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("bbb")); // before context
        assert!(result.output.contains("ccc")); // match
        assert!(result.output.contains("ddd")); // after context
    }

    #[tokio::test]
    async fn grep_invalid_regex() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(
                &json!({"action": "grep", "pattern": "[invalid"}),
                allow_token(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    #[tokio::test]
    async fn grep_skips_binary() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("text.txt"), "findme\n").unwrap();
        fs::write(dir.path().join("bin.dat"), b"findme\0binary").unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "grep", "pattern": "findme"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("text.txt"));
        assert!(!result.output.contains("bin.dat"));
    }

    #[tokio::test]
    async fn grep_include_filter() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "findme\n").unwrap();
        fs::write(dir.path().join("b.txt"), "findme\n").unwrap();
        let tool = make_tool(dir.path());
        let result = tool
            .execute(
                &json!({"action": "grep", "pattern": "findme", "include": "*.rs"}),
                allow_token(),
            )
            .await
            .unwrap();
        assert!(result.output.contains("a.rs"));
        assert!(!result.output.contains("b.txt"));
    }

    // --- path traversal ---

    #[tokio::test]
    async fn read_absolute_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(
                &json!({"action": "read", "path": "/etc/passwd"}),
                allow_token(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("relative"));
    }

    #[tokio::test]
    async fn read_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(
                &json!({"action": "read", "path": "../../../etc/passwd"}),
                allow_token(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("relative"));
    }

    #[tokio::test]
    async fn edit_absolute_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(
                &json!({
                    "action": "edit",
                    "path": "/etc/passwd",
                    "old_string": "root",
                    "new_string": "hacked"
                }),
                allow_token(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("relative"));
    }

    // --- unknown action ---

    #[tokio::test]
    async fn unknown_action_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let err = tool
            .execute(&json!({"action": "delete"}), allow_token())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown file action"));
    }

    // --- normalize_unicode ---

    #[test]
    fn normalize_smart_quotes() {
        let input = "say \u{201C}hello\u{201D} and \u{2018}bye\u{2019}";
        let output = normalize_unicode(input);
        assert_eq!(output, "say \"hello\" and 'bye'");
    }

    #[test]
    fn normalize_dashes() {
        let input = "a\u{2013}b\u{2014}c";
        let output = normalize_unicode(input);
        assert_eq!(output, "a-b-c");
    }

    #[test]
    fn normalize_spaces() {
        let input = "a\u{00A0}b\u{202F}c";
        let output = normalize_unicode(input);
        assert_eq!(output, "a b c");
    }

    // --- truncate_line ---

    #[test]
    fn truncate_short_line() {
        assert_eq!(truncate_line("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_line() {
        let long = "x".repeat(600);
        assert_eq!(truncate_line(&long, 500).len(), 500);
    }
}
