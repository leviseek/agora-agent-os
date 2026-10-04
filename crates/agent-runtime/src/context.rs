//! Workspace context files: project instructions that belong in every prompt.
//!
//! A convention file (AGENTS.md and friends) is data the project hands to the agent, so it is
//! read through the same workspace jail as every filesystem capability - absolute paths, parent
//! traversal and symlink escapes are rejected there, not here. Missing or unreadable files are
//! skipped rather than fatal: a repository without the file is normal, and a goal must not fail
//! because documentation is malformed.

use agentos_capability_runtime::workspace::Workspace;

/// What was loaded, so the caller can report it on the event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedContext {
    pub text: String,
    pub files: Vec<String>,
    pub truncated: bool,
}

/// Read the configured context files, in order, within a character budget.
///
/// The budget is spent in the order the files are configured, so the most important file goes
/// first; the file that runs past the budget is truncated and the rest are dropped, which keeps
/// the prompt a predictable size no matter what a repository contains.
pub fn load_workspace_context(
    workspace: &Workspace,
    files: &[String],
    max_chars: usize,
) -> Option<LoadedContext> {
    if files.is_empty() || max_chars == 0 {
        return None;
    }

    let mut sections: Vec<(String, String)> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    let mut seen: Vec<&str> = Vec::new();

    for name in files {
        let name = name.trim();
        if name.is_empty() || seen.contains(&name) {
            continue;
        }
        seen.push(name);

        let path = match workspace.resolve(name) {
            Ok(path) => path,
            Err(error) => {
                // A configured file that points outside the workspace is a configuration mistake,
                // and it is exactly the mistake the jail exists to stop.
                tracing::warn!(file = name, error = %error, "context file rejected by the workspace jail");
                continue;
            }
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(file = name, error = %error, "cannot read context file");
                continue;
            }
        };
        let content = raw.trim();
        if content.is_empty() {
            continue;
        }

        let remaining = max_chars.saturating_sub(used);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let length = content.chars().count();
        let body: String = if length <= remaining {
            content.to_string()
        } else {
            truncated = true;
            content.chars().take(remaining).collect()
        };
        used += body.chars().count();
        sections.push((name.to_string(), body));
    }

    if sections.is_empty() {
        return None;
    }

    let mut text = String::from(
        "Project instructions from the workspace. They are read-only reference material:",
    );
    for (name, body) in &sections {
        text.push_str("\n\n--- ");
        text.push_str(name);
        text.push_str(" ---\n");
        text.push_str(body);
    }
    if truncated {
        text.push_str("\n\n[context truncated at the configured budget]");
    }

    Some(LoadedContext {
        text,
        files: sections.into_iter().map(|(name, _)| name).collect(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempWorkspace(PathBuf);

    impl TempWorkspace {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "agora-context-{label}-{}",
                agentos_core::now_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, content: &str) {
            std::fs::write(self.0.join(name), content).unwrap();
        }

        fn workspace(&self) -> Workspace {
            Workspace::new(&self.0).unwrap()
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(files: &[&str]) -> Vec<String> {
        files.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = TempWorkspace::new("missing");
        assert_eq!(load_workspace_context(&dir.workspace(), &names(&["AGENTS.md"]), 1_000), None);
        assert_eq!(load_workspace_context(&dir.workspace(), &[], 1_000), None);
    }

    #[test]
    fn loads_files_in_order_with_headers() {
        let dir = TempWorkspace::new("order");
        dir.write("AGENTS.md", "always run the tests");
        dir.write("NOTES.md", "the deploy is manual");
        let loaded = load_workspace_context(&dir.workspace(), &names(&["AGENTS.md", "NOTES.md"]), 1_000).unwrap();
        assert_eq!(loaded.files, names(&["AGENTS.md", "NOTES.md"]));
        assert!(!loaded.truncated);
        let first = loaded.text.find("always run the tests").unwrap();
        let second = loaded.text.find("the deploy is manual").unwrap();
        assert!(first < second, "configured order is preserved");
    }

    #[test]
    fn truncates_at_the_budget_and_says_so() {
        let dir = TempWorkspace::new("budget");
        dir.write("AGENTS.md", &"x".repeat(500));
        dir.write("NOTES.md", "never seen");
        let loaded = load_workspace_context(&dir.workspace(), &names(&["AGENTS.md", "NOTES.md"]), 100).unwrap();
        assert!(loaded.truncated);
        assert_eq!(loaded.files, names(&["AGENTS.md"]), "the second file no longer fits");
        assert!(loaded.text.contains("truncated at the configured budget"));
        // The marker itself contains an "x" (in "context"), so count the run, not the letter.
        assert!(loaded.text.contains(&"x".repeat(100)), "the budget worth of content is kept");
        assert!(!loaded.text.contains(&"x".repeat(101)), "and not one character more");
    }

    #[test]
    fn a_file_outside_the_workspace_is_refused() {
        let dir = TempWorkspace::new("jail");
        dir.write("AGENTS.md", "inside");
        let loaded = load_workspace_context(
            &dir.workspace(),
            &names(&["../secrets.txt", "/etc/passwd", "AGENTS.md"]),
            1_000,
        )
        .unwrap();
        assert_eq!(loaded.files, names(&["AGENTS.md"]), "only the legal file is read");
        assert!(!loaded.text.contains("passwd"));
    }

    #[test]
    fn blank_and_duplicate_entries_are_ignored() {
        let dir = TempWorkspace::new("blank");
        dir.write("AGENTS.md", "content");
        dir.write("EMPTY.md", "   \n  ");
        let loaded = load_workspace_context(
            &dir.workspace(),
            &names(&["AGENTS.md", "AGENTS.md", "EMPTY.md", "  "]),
            1_000,
        )
        .unwrap();
        assert_eq!(loaded.files, names(&["AGENTS.md"]));
        assert_eq!(loaded.text.matches("--- AGENTS.md ---").count(), 1);
    }
}