//! Shared path validation helpers for workspace containment.
//!
//! Used by the file tool, WASM sandbox, and container sandbox to enforce
//! that all file access stays within the workspace root.

use std::path::{Component, Path, PathBuf};

use crate::error::CherubError;

/// Validate that `path` is safe for filesystem access.
///
/// Rejects:
/// - Empty paths
/// - Paths starting with `/` (absolute)
/// - Paths containing `..` (directory traversal)
/// - Paths containing null bytes
pub fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.starts_with('/') {
        return false;
    }
    if path.contains('\0') {
        return false;
    }
    for component in Path::new(path).components() {
        match component {
            Component::ParentDir | Component::RootDir => return false,
            _ => {}
        }
    }
    true
}

/// Resolve a relative path against the workspace root, with containment checks.
///
/// 1. Validates the path is safe (no traversal, no absolute).
/// 2. Joins with workspace root.
/// 3. Canonicalizes (resolves symlinks).
/// 4. Verifies the resolved path is still inside the workspace root.
///
/// Returns the canonicalized absolute path on success.
pub fn resolve_workspace_path(workspace_root: &Path, path: &str) -> Result<PathBuf, CherubError> {
    if !is_safe_relative_path(path) {
        return Err(CherubError::ToolExecution(
            "path must be relative and must not contain '..' or null bytes".to_owned(),
        ));
    }

    let joined = workspace_root.join(path);

    // Canonicalize resolves symlinks. If the file doesn't exist yet (edit creating
    // a new file), fall back to canonicalizing the parent directory.
    let canonical = if joined.exists() {
        joined
            .canonicalize()
            .map_err(|e| CherubError::ToolExecution(format!("failed to resolve path: {e}")))?
    } else {
        // For non-existent files, canonicalize the parent and append the filename.
        let parent = joined
            .parent()
            .ok_or_else(|| CherubError::ToolExecution("path has no parent directory".to_owned()))?;
        if !parent.exists() {
            return Err(CherubError::ToolExecution(
                "parent directory does not exist".to_owned(),
            ));
        }
        let canonical_parent = parent
            .canonicalize()
            .map_err(|e| CherubError::ToolExecution(format!("failed to resolve parent: {e}")))?;
        let file_name = joined
            .file_name()
            .ok_or_else(|| CherubError::ToolExecution("path has no file name".to_owned()))?;
        canonical_parent.join(file_name)
    };

    // Containment check: resolved path must be inside workspace root.
    let canonical_root = workspace_root.canonicalize().map_err(|e| {
        CherubError::ToolExecution(format!("failed to resolve workspace root: {e}"))
    })?;

    if !canonical.starts_with(&canonical_root) {
        return Err(CherubError::ToolExecution(
            "path escapes workspace root".to_owned(),
        ));
    }

    Ok(canonical)
}

/// Detect binary content by checking for null bytes in the first 8 KiB.
pub fn is_binary_content(data: &[u8]) -> bool {
    let check_len = data.len().min(8192);
    data[..check_len].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_safe_relative_path ---

    #[test]
    fn rejects_empty() {
        assert!(!is_safe_relative_path(""));
    }

    #[test]
    fn rejects_absolute() {
        assert!(!is_safe_relative_path("/etc/passwd"));
    }

    #[test]
    fn rejects_traversal() {
        assert!(!is_safe_relative_path("../etc/passwd"));
        assert!(!is_safe_relative_path("foo/../../etc/passwd"));
    }

    #[test]
    fn rejects_null_bytes() {
        assert!(!is_safe_relative_path("foo\0bar"));
    }

    #[test]
    fn allows_normal_paths() {
        assert!(is_safe_relative_path("src/main.rs"));
        assert!(is_safe_relative_path("file.txt"));
        assert!(is_safe_relative_path("a/b/c/d.txt"));
    }

    #[test]
    fn allows_dot_prefix() {
        assert!(is_safe_relative_path(".gitignore"));
        assert!(is_safe_relative_path(".config/nextest.toml"));
    }

    // --- resolve_workspace_path ---

    #[test]
    fn resolve_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_workspace_path(dir.path(), "/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("relative"));
    }

    #[test]
    fn resolve_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_workspace_path(dir.path(), "../escape").unwrap_err();
        assert!(err.to_string().contains("relative"));
    }

    #[test]
    fn resolve_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();
        let resolved = resolve_workspace_path(dir.path(), "test.txt").unwrap();
        assert!(resolved.ends_with("test.txt"));
        assert!(resolved.starts_with(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn resolve_nonexistent_file_in_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_workspace_path(dir.path(), "new_file.txt").unwrap();
        assert!(resolved.ends_with("new_file.txt"));
    }

    #[test]
    fn resolve_nonexistent_parent() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_workspace_path(dir.path(), "no_such_dir/file.txt").unwrap_err();
        assert!(err.to_string().contains("parent directory does not exist"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_symlink_escape_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let escape_target = tempfile::tempdir().unwrap();
        std::fs::write(escape_target.path().join("secret.txt"), "secret").unwrap();

        // Create a symlink inside workspace that points outside.
        std::os::unix::fs::symlink(escape_target.path(), dir.path().join("escape")).unwrap();

        let err = resolve_workspace_path(dir.path(), "escape/secret.txt").unwrap_err();
        assert!(err.to_string().contains("escapes workspace root"));
    }

    // --- is_binary_content ---

    #[test]
    fn text_is_not_binary() {
        assert!(!is_binary_content(b"Hello, world!\nLine two\n"));
    }

    #[test]
    fn null_byte_is_binary() {
        assert!(is_binary_content(b"Hello\0world"));
    }

    #[test]
    fn empty_is_not_binary() {
        assert!(!is_binary_content(b""));
    }

    #[test]
    fn null_at_8k_boundary() {
        let mut data = vec![b'x'; 8192];
        data[8191] = 0;
        assert!(is_binary_content(&data));
    }

    #[test]
    fn null_beyond_8k_not_detected() {
        let mut data = vec![b'x'; 16384];
        data[8192] = 0;
        assert!(!is_binary_content(&data));
    }
}
