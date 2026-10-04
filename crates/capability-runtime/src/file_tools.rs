//! File tools an agent needs to actually work on a codebase: patch a file, find things in it.
//!
//! Both go through the workspace jail and the same policy seam as every other capability, so a
//! deployment that has not granted filesystem write simply cannot call the edit tool.

use crate::capability::{Capability, CapabilityContext};
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{CapabilityDescriptor, CapabilityKind, CapabilityPermission};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Patch a file by replacing an exact snippet.
///
/// The uniqueness requirement is the point: a model that says "replace this line" without enough
/// context gets an error instead of a silent edit in the wrong place. Rewriting whole files is
/// what filesystem-write is for.
pub struct FilesystemEditCapability {
    max_bytes: u64,
}

impl FilesystemEditCapability {
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

#[async_trait]
impl Capability for FilesystemEditCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        let mut d = crate::builtins::descriptor(
            "filesystem-edit",
            "1.0.0",
            "Replace an exact snippet in a workspace file. The snippet must be unique unless replace_all is set. Requires an explicit policy grant.",
            CapabilityKind::Builtin,
            &["fs", "workspace", "write", "patch", "mutating"],
            json!({
                "type": "object",
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "minLength": 1, "maxLength": 512 },
                    "old_string": { "type": "string", "minLength": 1, "maxLength": 262144 },
                    "new_string": { "type": "string", "maxLength": 262144 },
                    "replace_all": { "type": "boolean" }
                }
            }),
            json!({
                "type": "object",
                "required": ["path", "replacements", "bytes"],
                "properties": {
                    "path": { "type": "string" },
                    "replacements": { "type": "number" },
                    "bytes": { "type": "number" }
                }
            }),
            CapabilityPermission::read_only_fs().with_fs_write(),
        );
        d.idempotent = false;
        d
    }

    async fn invoke(&self, input: Value, ctx: CapabilityContext) -> Result<Value> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::invalid_input("filesystem-edit requires a path field"))?;
        let old_string = input
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::invalid_input("filesystem-edit requires old_string"))?;
        let new_string = input.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
        let replace_all = input.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ctx.permission.fs_write {
            return Err(RuntimeError::policy_denied("filesystem write permission was not granted"));
        }
        if old_string.is_empty() {
            return Err(RuntimeError::invalid_input("old_string must not be empty"));
        }
        if old_string == new_string {
            return Err(RuntimeError::invalid_input(
                "old_string and new_string are identical: there is nothing to change",
            ));
        }

        let content = ctx.workspace.read_to_string(path, self.max_bytes).await?;
        let occurrences = content.matches(old_string).count();
        if occurrences == 0 {
            return Err(RuntimeError::invalid_input(format!(
                "old_string was not found in {path}: read the file and copy the exact text, whitespace included"
            )));
        }
        if occurrences > 1 && !replace_all {
            // Editing the wrong occurrence is worse than not editing at all.
            return Err(RuntimeError::invalid_input(format!(
                "old_string appears {occurrences} times in {path}: include more surrounding text, or set replace_all"
            )));
        }
        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        let bytes = ctx.workspace.write(path, &updated, self.max_bytes).await?;
        Ok(json!({
            "path": path,
            "replacements": if replace_all { occurrences } else { 1 },
            "bytes": bytes,
        }))
    }
}

/// Match a path against a glob: * within a segment, ** across segments, ? one character.
///
/// Hand written on purpose: a regex engine is a large dependency to carry for the three wildcards
/// a file search actually needs, and this one is small enough to test exhaustively.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let path: Vec<char> = path.chars().collect();
    matches_from(&pattern, &path)
}

fn matches_from(pattern: &[char], path: &[char]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    match pattern[0] {
        ASTERISK => {
            let deep = pattern.len() > 1 && pattern[1] == ASTERISK;
            if deep {
                // "**/" also matches zero directories, so the separator is part of the wildcard
                // rather than something the path must supply.
                let rest = if pattern.len() > 2 && pattern[2] == SEPARATOR {
                    &pattern[3..]
                } else {
                    &pattern[2..]
                };
                return (0..=path.len()).any(|index| matches_from(rest, &path[index..]));
            }
            // A single star stays inside one path segment.
            let rest = &pattern[1..];
            if matches_from(rest, path) {
                return true;
            }
            let mut index = 0;
            while index < path.len() {
                if path[index] == SEPARATOR {
                    return false;
                }
                index += 1;
                if matches_from(rest, &path[index..]) {
                    return true;
                }
            }
            false
        }
        QUESTION => !path.is_empty() && path[0] != SEPARATOR && matches_from(&pattern[1..], &path[1..]),
        literal => !path.is_empty() && path[0] == literal && matches_from(&pattern[1..], &path[1..]),
    }
}

const ASTERISK: char = '*';
const QUESTION: char = '?';
const SEPARATOR: char = '/';

/// Find files by glob and lines by text, inside the workspace only.
pub struct FilesystemSearchCapability {
    max_results: usize,
    max_files: usize,
    max_file_bytes: u64,
}

impl FilesystemSearchCapability {
    pub fn new(max_results: usize, max_files: usize, max_file_bytes: u64) -> Self {
        Self { max_results, max_files, max_file_bytes }
    }
}

impl Default for FilesystemSearchCapability {
    fn default() -> Self {
        Self::new(200, 2_000, 262_144)
    }
}

#[async_trait]
impl Capability for FilesystemSearchCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        crate::builtins::descriptor(
            "filesystem-search",
            "1.0.0",
            "Find files by glob and lines by text inside the workspace. Matching is a case-insensitive substring by default, not a regular expression.",
            CapabilityKind::Builtin,
            &["fs", "workspace", "search", "read-only"],
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "root": { "type": "string", "maxLength": 512 },
                    "glob": { "type": "string", "maxLength": 256 },
                    "query": { "type": "string", "maxLength": 512 },
                    "case_sensitive": { "type": "boolean" },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 1000 }
                }
            }),
            json!({
                "type": "object",
                "required": ["matches", "files_scanned", "truncated"],
                "properties": {
                    "matches": { "type": "array" },
                    "files_scanned": { "type": "number" },
                    "truncated": { "type": "boolean" }
                }
            }),
            CapabilityPermission::read_only_fs(),
        )
    }

    async fn invoke(&self, input: Value, ctx: CapabilityContext) -> Result<Value> {
        let root = input.get("root").and_then(|v| v.as_str()).unwrap_or(".");
        let glob = input.get("glob").and_then(|v| v.as_str()).unwrap_or("**");
        let query = input.get("query").and_then(|v| v.as_str()).filter(|q| !q.is_empty());
        let case_sensitive = input.get("case_sensitive").and_then(|v| v.as_bool()).unwrap_or(false);
        let limit = input
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(self.max_results)
            .min(self.max_results);
        let needle = query.map(|q| if case_sensitive { q.to_string() } else { q.to_lowercase() });

        // Resolve the root through the jail before walking anything.
        let start = ctx.workspace.resolve(root)?;
        let mut queue = vec![(start, String::new())];
        let mut matches: Vec<Value> = Vec::new();
        let mut files_scanned = 0usize;
        let mut truncated = false;

        while let Some((dir, prefix)) = queue.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            while let Some(entry) = entries.next_entry().await? {
                if matches.len() >= limit || files_scanned >= self.max_files {
                    truncated = true;
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let relative = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                let meta = entry.metadata().await?;
                if meta.is_dir() {
                    // Hidden directories are noise for a code search, and .git can be huge.
                    if !name.starts_with('.') {
                        queue.push((entry.path(), relative));
                    }
                    continue;
                }
                if !meta.is_file() || !glob_match(glob, &relative) {
                    continue;
                }
                files_scanned += 1;
                let Some(needle) = &needle else {
                    matches.push(json!({ "path": relative, "line": 0, "text": "" }));
                    continue;
                };
                if meta.len() > self.max_file_bytes {
                    continue;
                }
                let Ok(bytes) = tokio::fs::read(entry.path()).await else { continue };
                // A NUL byte in the first block is the cheap, reliable "this is not text" test.
                if bytes.iter().take(8192).any(|b| *b == 0) {
                    continue;
                }
                let Ok(text) = String::from_utf8(bytes) else { continue };
                for (index, line) in text.lines().enumerate() {
                    let haystack = if case_sensitive { line.to_string() } else { line.to_lowercase() };
                    if haystack.contains(needle.as_str()) {
                        matches.push(json!({
                            "path": relative,
                            "line": index + 1,
                            "text": line.chars().take(400).collect::<String>(),
                        }));
                        if matches.len() >= limit {
                            truncated = true;
                            break;
                        }
                    }
                }
            }
            if matches.len() >= limit || files_scanned >= self.max_files {
                truncated = true;
                break;
            }
        }

        Ok(json!({ "matches": matches, "files_scanned": files_scanned, "truncated": truncated }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CallerContext;
    use crate::workspace::Workspace;
    use agentos_core::{CapabilityId, SessionId};
    use std::sync::Arc;

    fn tmpdir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agentos-filetools-{label}-{}", agentos_core::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(permission: CapabilityPermission, root: &std::path::Path) -> CapabilityContext {
        CapabilityContext {
            capability_id: CapabilityId::new(),
            caller: CallerContext::new(SessionId::new()),
            permission,
            workspace: Arc::new(Workspace::new(root).unwrap()),
            artifacts: None,
            timeout_ms: 1000,
        }
    }

    fn granted() -> CapabilityPermission {
        CapabilityPermission::read_only_fs().with_fs_write()
    }

    #[tokio::test]
    async fn edit_replaces_a_unique_snippet() {
        let dir = tmpdir("edit-ok");
        std::fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = FilesystemEditCapability::new(4096)
            .invoke(
                json!({ "path": "a.txt", "old_string": "beta", "new_string": "BETA" }),
                ctx(granted(), &dir),
            )
            .await
            .unwrap();
        assert_eq!(out["replacements"], 1);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "alpha\nBETA\ngamma\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_refuses_an_ambiguous_snippet_but_replace_all_wins() {
        let dir = tmpdir("edit-ambiguous");
        std::fs::write(dir.join("a.txt"), "same\nsame\n").unwrap();
        let error = FilesystemEditCapability::new(4096)
            .invoke(
                json!({ "path": "a.txt", "old_string": "same", "new_string": "different" }),
                ctx(granted(), &dir),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("2 times"), "got: {}", error.message);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "same\nsame\n",
            "a refused edit changes nothing"
        );

        let out = FilesystemEditCapability::new(4096)
            .invoke(
                json!({ "path": "a.txt", "old_string": "same", "new_string": "different", "replace_all": true }),
                ctx(granted(), &dir),
            )
            .await
            .unwrap();
        assert_eq!(out["replacements"], 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_reports_a_missing_snippet_and_a_pointless_request() {
        let dir = tmpdir("edit-missing");
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        let missing = FilesystemEditCapability::new(4096)
            .invoke(
                json!({ "path": "a.txt", "old_string": "nope", "new_string": "x" }),
                ctx(granted(), &dir),
            )
            .await
            .unwrap_err();
        assert!(missing.message.contains("was not found"), "got: {}", missing.message);

        let noop = FilesystemEditCapability::new(4096)
            .invoke(
                json!({ "path": "a.txt", "old_string": "hello", "new_string": "hello" }),
                ctx(granted(), &dir),
            )
            .await
            .unwrap_err();
        assert!(noop.message.contains("identical"), "got: {}", noop.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_obeys_the_policy_and_the_jail() {
        let dir = tmpdir("edit-jail");
        std::fs::write(dir.join("a.txt"), "content\n").unwrap();
        let denied = FilesystemEditCapability::new(4096)
            .invoke(
                json!({ "path": "a.txt", "old_string": "content", "new_string": "changed" }),
                ctx(CapabilityPermission::read_only_fs(), &dir),
            )
            .await
            .unwrap_err();
        assert_eq!(denied.kind, agentos_core::ErrorKind::PolicyDenied);

        let escaped = FilesystemEditCapability::new(4096)
            .invoke(
                json!({ "path": "../outside.txt", "old_string": "a", "new_string": "b" }),
                ctx(granted(), &dir),
            )
            .await;
        assert!(escaped.is_err(), "the jail refuses a path outside the workspace");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_matching_covers_the_three_wildcards() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"), "a single star stays inside one segment");
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "main.rs"), "** may match nothing at all");
        assert!(glob_match("src/**", "src/a/b/c.rs"));
        assert!(glob_match("ma?n.rs", "main.rs"));
        assert!(!glob_match("ma?n.rs", "man.rs"));
        assert!(glob_match("*", "main.rs"));
        assert!(!glob_match("*", "src/main.rs"));
    }

    #[tokio::test]
    async fn search_finds_files_and_lines() {
        let dir = tmpdir("search");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "fn main() {}\nlet secret = 42;\n").unwrap();
        std::fs::write(dir.join("README.md"), "nothing here\n").unwrap();
        let cap = FilesystemSearchCapability::default();

        let files = cap
            .invoke(json!({ "glob": "**/*.rs" }), ctx(CapabilityPermission::read_only_fs(), &dir))
            .await
            .unwrap();
        assert_eq!(files["matches"].as_array().unwrap().len(), 1);
        assert_eq!(files["matches"][0]["path"], "src/lib.rs");

        let lines = cap
            .invoke(
                json!({ "glob": "**", "query": "SECRET" }),
                ctx(CapabilityPermission::read_only_fs(), &dir),
            )
            .await
            .unwrap();
        assert_eq!(lines["matches"].as_array().unwrap().len(), 1, "case-insensitive by default");
        assert_eq!(lines["matches"][0]["line"], 2);
        assert_eq!(lines["matches"][0]["text"], "let secret = 42;");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_is_bounded_and_stays_inside_the_workspace() {
        let dir = tmpdir("search-bounds");
        for index in 0..10 {
            std::fs::write(dir.join(format!("f{index}.txt")), "needle\n").unwrap();
        }
        let cap = FilesystemSearchCapability::new(3, 100, 4096);
        let out = cap
            .invoke(
                json!({ "glob": "*.txt", "query": "needle" }),
                ctx(CapabilityPermission::read_only_fs(), &dir),
            )
            .await
            .unwrap();
        assert_eq!(out["matches"].as_array().unwrap().len(), 3);
        assert_eq!(out["truncated"], true);

        let escaped = cap
            .invoke(json!({ "root": "../.." }), ctx(CapabilityPermission::read_only_fs(), &dir))
            .await;
        assert!(escaped.is_err(), "searching outside the workspace is refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_skips_binary_and_oversized_files() {
        let dir = tmpdir("search-binary");
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        std::fs::write(dir.join("big.txt"), "needle".repeat(200)).unwrap();
        std::fs::write(dir.join("small.txt"), "needle\n").unwrap();
        let cap = FilesystemSearchCapability::new(50, 100, 64);
        let out = cap
            .invoke(
                json!({ "glob": "*", "query": "needle" }),
                ctx(CapabilityPermission::read_only_fs(), &dir),
            )
            .await
            .unwrap();
        let paths: Vec<String> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths, vec!["small.txt".to_string()], "binary and oversized files are skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
