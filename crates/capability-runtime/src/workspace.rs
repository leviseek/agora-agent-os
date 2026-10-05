//! The workspace jail: every filesystem access a capability makes goes through here.
//!
//! Rules, all enforced before any IO happens:
//! 1. relative paths only - absolute paths and drive prefixes are rejected,
//! 2. no parent-directory components,
//! 3. the resolved path must stay inside the workspace root after canonicalization, which also
//!    defeats symlink escapes,
//! 4. reads and writes are additionally gated by the capability permission set.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::WorkspaceRecord;
use agentos_core::WorkspaceId;
use agentos_storage::store::{collections, Collection, Store};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

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

/// Turns "which workspace is this session in?" into "which directory may it touch?".
///
/// The mesh holds one of these rather than one jail, because one node hosts sessions from many
/// workspaces and each must be jail-rooted in its own directory. `None` is a session with no
/// workspace - a record written before workspaces existed - and keeps the node's root as its jail,
/// which is exactly where its files already are. That keeps an upgrade from moving anybody's files;
/// it is also the one case where a session can still see a sibling workspace's directory, and it
/// disappears once every session has a workspace (see docs/decisions.md D20).
#[async_trait]
pub trait WorkspaceResolver: Send + Sync + std::fmt::Debug {
    async fn resolve(&self, workspace: Option<&WorkspaceId>) -> Result<Arc<Workspace>>;
}

/// One fixed jail, for tests and for a runtime that has a single workspace.
#[derive(Debug)]
pub struct FixedWorkspaceResolver {
    workspace: Arc<Workspace>,
}

impl FixedWorkspaceResolver {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl WorkspaceResolver for FixedWorkspaceResolver {
    async fn resolve(&self, _workspace: Option<&WorkspaceId>) -> Result<Arc<Workspace>> {
        Ok(self.workspace.clone())
    }
}

/// The real thing: a node root, one directory per workspace, jails created on first use and kept.
///
/// A workspace's directory is **chosen**, not derived: it is whatever the record says (falling back
/// to the id for a record written before that was a choice). The directory must stay inside the
/// node's root - a workspace rooted at the root itself, or outside it, would be a way to read every
/// other workspace's files, which is the whole thing this exists to prevent.
pub struct WorkspaceRegistry {
    root: PathBuf,
    legacy: Arc<Workspace>,
    /// Where the workspace records live, so an id can be turned into its directory. Absent in a
    /// runtime wired without storage (and in tests): then a workspace is jailed by its id, which is
    /// where workspaces created before directories were a choice already live.
    store: Option<Arc<dyn Store>>,
    /// Workspace id -> the directory its record names, learned from the store once per workspace.
    directories: RwLock<HashMap<String, String>>,
    /// Canonical directory -> jail. Keyed by the directory, not the id: the directory is what the
    /// jail *is*, so a call naming a different one is a different jail (or a refusal) rather than a
    /// cache hit that quietly returns somebody else's folder.
    jails: RwLock<HashMap<String, Arc<Workspace>>>,
}

// Written out rather than derived: a store handle is not inspectable, and a `Debug` that printed one
// would be noise on every line that logs a resolver.
impl std::fmt::Debug for WorkspaceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceRegistry")
            .field("root", &self.root)
            .field("has_store", &self.store.is_some())
            .field("known_workspaces", &self.directories.read().len())
            .field("jails", &self.jails.read().len())
            .finish()
    }
}

impl WorkspaceRegistry {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let legacy = Arc::new(Workspace::new(root.as_ref())?);
        Ok(Self {
            root: legacy.root().to_path_buf(),
            legacy,
            store: None,
            directories: RwLock::new(HashMap::new()),
            jails: RwLock::new(HashMap::new()),
        })
    }

    /// Read the workspace records from this store to learn each workspace's directory.
    pub fn with_store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory a workspace works in, as its record says it (or its id, for a record written
    /// before the directory was a choice).
    pub async fn directory_of(&self, workspace: &WorkspaceId) -> Option<String> {
        let store = self.store.as_ref()?;
        let collection: Collection<WorkspaceRecord> = Collection::new(collections::WORKSPACES);
        let record = collection.load(store.as_ref(), workspace.as_str()).await.ok().flatten()?;
        Some(record.directory_name().to_string())
    }

    /// The jail for a chosen directory. Created on first use and cached by the directory it
    /// resolves to, because constructing a jail canonicalises a path on disk and doing that on every
    /// capability call would be an IO operation per call for no gain. A workspace's directory never
    /// changes - renaming a workspace renames nothing on disk - so the cache cannot go stale.
    pub fn jail_for(&self, directory: &str) -> Result<Arc<Workspace>> {
        let jail = Arc::new(self.open_jail(directory)?);
        let key = jail.root().to_string_lossy().to_string();
        if let Some(existing) = self.jails.read().get(&key) {
            return Ok(existing.clone());
        }
        self.jails.write().insert(key, jail.clone());
        Ok(jail)
    }

    /// Open (creating if needed) the jail for a directory, and refuse anything that is not a
    /// strict descendant of the node root.
    fn open_jail(&self, directory: &str) -> Result<Workspace> {
        let requested = Path::new(directory);
        if requested.components().any(|part| matches!(part, Component::ParentDir)) {
            return Err(RuntimeError::policy_denied(format!(
                "a workspace directory may not climb out of the node root: {directory}"
            )));
        }
        let target = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };
        let jail = Workspace::new(&target)?;
        if jail.root() == self.root {
            return Err(RuntimeError::policy_denied(format!(
                "a workspace directory must be inside the node root, not the root itself: {directory}"
            )));
        }
        if !jail.root().starts_with(&self.root) {
            return Err(RuntimeError::policy_denied(format!(
                "a workspace directory must live under {}: {directory}",
                self.root.display()
            )));
        }
        Ok(jail)
    }
}

#[async_trait]
impl WorkspaceResolver for WorkspaceRegistry {
    async fn resolve(&self, workspace: Option<&WorkspaceId>) -> Result<Arc<Workspace>> {
        let Some(id) = workspace else {
            return Ok(self.legacy.clone());
        };
        if let Some(directory) = self.directories.read().get(id.as_str()).cloned() {
            return self.jail_for(&directory);
        }
        let directory = match self.directory_of(id).await {
            Some(directory) => directory,
            // No store at all (tests, a runtime wired without storage): jail by id, which is where a
            // workspace created before directories were a choice lives. With a store, an id nobody
            // created is not a workspace - refusing is better than handing it a directory the record
            // does not name.
            None if self.store.is_none() => id.as_str().to_string(),
            None => {
                return Err(RuntimeError::not_found(format!("workspace {id} does not exist"))
                    .with_detail("workspace_id", id.as_str()))
            }
        };
        self.directories
            .write()
            .insert(id.as_str().to_string(), directory.clone());
        self.jail_for(&directory)
    }
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

    #[tokio::test]
    async fn each_workspace_gets_its_own_jail_under_the_root() {
        let dir = std::env::temp_dir().join(format!("agentos-registry-{}", agentos_core::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = WorkspaceRegistry::new(&dir).unwrap();

        // The jail follows the chosen directory, not the id.
        let jail_a = registry.jail_for("projects/a").unwrap();
        let jail_b = registry.jail_for("projects/b").unwrap();
        assert!(jail_a.root().starts_with(std::fs::canonicalize(&dir).unwrap()));
        assert!(jail_a.root().ends_with("projects/a"));
        assert_ne!(jail_a.root(), jail_b.root());

        // A directory that leaves the root, or is the root, is not a workspace.
        assert!(registry.jail_for("../escape").is_err());
        assert!(registry.jail_for(".").is_err());

        // This is the whole point: A writing a file does not make it visible to B, and a relative
        // path out of A's jail is refused before any IO happens.
        jail_a.write("only-a.txt", "alice", 1024).await.unwrap();
        assert!(jail_b.read_to_string("only-a.txt", 1024).await.is_err());
        assert!(jail_a.resolve("../only-a.txt").is_err());

        // A session with no workspace keeps the node root, where its files already are.
        let legacy = registry.resolve(None).await.unwrap();
        assert_eq!(legacy.root(), std::fs::canonicalize(&dir).unwrap());
    }
}
