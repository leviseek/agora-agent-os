//! The workspace jail: every filesystem access a capability makes goes through here.
//!
//! Rules, all enforced before any IO happens:
//! 1. relative paths only - absolute paths and drive prefixes are rejected,
//! 2. no parent-directory components,
//! 3. the resolved path must stay inside the workspace root after canonicalization, which also
//!    defeats symlink escapes,
//! 4. reads and writes are additionally gated by the capability permission set.

use agentos_core::error::{Result, RuntimeError};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Canonicalize the root once, so later prefix checks compare like with like.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)?;
        let canonical = std::fs::canonicalize(root).map_err(|e| {
            RuntimeError::invalid_input(format!("workspace root {} is unusable: {e}", root.display()))
        })?;
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Turn a user-supplied relative path into a safe absolute path.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf> {
        if requested.trim().is_empty() {
            return Err(RuntimeError::invalid_input("path must not be empty"));
        }
        let candidate = Path::new(requested);
        if candidate.is_absolute() {
            return Err(RuntimeError::policy_denied(format!(
                "absolute paths are not allowed in the workspace: {requested}"
            )));
        }
        for component in candidate.components() {
            match component {
                Component::ParentDir => {
                    return Err(RuntimeError::policy_denied(format!(
                        "path traversal is not allowed: {requested}"
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(RuntimeError::policy_denied(format!(
                        "path escapes the workspace: {requested}"
                    )));
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }

        let joined = self.root.join(candidate);
        // Canonicalize the deepest existing ancestor and require it to stay inside the root.
        // This is what stops a symlink inside the workspace from pointing outside.
        let mut probe = joined.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            match std::fs::canonicalize(&probe) {
                Ok(real) => {
                    if !real.starts_with(&self.root) {
                        return Err(RuntimeError::policy_denied(format!(
                            "path escapes the workspace: {requested}"
                        )));
                    }
                    let mut resolved = real;
                    for part in tail.iter().rev() {
                        resolved.push(part);
                    }
                    if !resolved.starts_with(&self.root) {
                        return Err(RuntimeError::policy_denied(format!(
                            "path escapes the workspace: {requested}"
                        )));
                    }
                    return Ok(resolved);
                }
                Err(_) => match probe.parent() {
                    Some(parent) if parent.starts_with(&self.root) => {
                        if let Some(name) = probe.file_name() {
                            tail.push(name.to_os_string());
                        }
                        probe = parent.to_path_buf();
                    }
                    _ => {
                        return Err(RuntimeError::policy_denied(format!(
                            "path escapes the workspace: {requested}"
                        )));
                    }
                },
            }
        }
    }

    pub async fn read_to_string(&self, requested: &str, max_bytes: u64) -> Result<String> {
        let path = self.resolve(requested)?;
        let meta = tokio::fs::metadata(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RuntimeError::not_found(format!("file not found: {requested}"))
            } else {
                e.into()
            }
        })?;
        if !meta.is_file() {
            return Err(RuntimeError::invalid_input(format!("not a file: {requested}")));
        }
        if meta.len() > max_bytes {
            return Err(RuntimeError::invalid_input(format!(
                "file {requested} is {} bytes, limit is {max_bytes}",
                meta.len()
            )));
        }
        Ok(tokio::fs::read_to_string(&path).await?)
    }

    pub async fn list(&self, requested: &str, limit: usize) -> Result<Vec<WorkspaceEntry>> {
        let path = self.resolve(requested)?;
        let mut rd = tokio::fs::read_dir(&path).await?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            let meta = entry.metadata().await?;
            out.push(WorkspaceEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                is_dir: meta.is_dir(),
                size: meta.len(),
            });
            if out.len() >= limit {
                break;
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub async fn write(&self, requested: &str, contents: &str, max_bytes: u64) -> Result<u64> {
        if contents.len() as u64 > max_bytes {
            return Err(RuntimeError::invalid_input(format!(
                "refusing to write {} bytes, limit is {max_bytes}",
                contents.len()
            )));
        }
        let path = self.resolve(requested)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, contents).await?;
        Ok(contents.len() as u64)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (Workspace, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agentos-ws-{}", agentos_core::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        (Workspace::new(&dir).unwrap(), dir)
    }

    #[test]
    fn relative_paths_resolve_inside_the_root() {
        let (w, dir) = ws();
        let p = w.resolve("notes/today.txt").unwrap();
        assert!(p.starts_with(std::fs::canonicalize(&dir).unwrap()));
    }

    #[test]
    fn traversal_is_denied() {
        let (w, _d) = ws();
        assert!(w.resolve("../secrets.txt").is_err());
        assert!(w.resolve("a/../../b").is_err());
    }

    #[test]
    fn absolute_paths_are_denied() {
        let (w, _d) = ws();
        assert!(w.resolve("C:\\Windows\\win.ini").is_err());
        assert!(w.resolve("/etc/passwd").is_err());
    }

    #[test]
    fn empty_paths_are_denied() {
        let (w, _d) = ws();
        assert!(w.resolve("   ").is_err());
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let (w, _d) = ws();
        w.write("a/b.txt", "hello", 1024).await.unwrap();
        assert_eq!(w.read_to_string("a/b.txt", 1024).await.unwrap(), "hello");
        let entries = w.list("a", 10).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "b.txt");
    }

    #[tokio::test]
    async fn oversized_reads_are_rejected() {
        let (w, _d) = ws();
        w.write("big.txt", "0123456789", 1024).await.unwrap();
        assert!(w.read_to_string("big.txt", 4).await.is_err());
    }
}
