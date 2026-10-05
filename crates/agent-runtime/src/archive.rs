//! Archiving a conversation: one self-contained package, and getting it back.
//!
//! Archiving is not deleting and not closing. Closing stops the actor and keeps the session in the
//! live store; archiving writes everything the conversation consists of into a single file under an
//! archive root and marks the record `archived`, so the hot store stops growing with conversations
//! nobody is having any more. Nothing is lost: the package is the conversation.
//!
//! What goes in, and why each part is there:
//!
//!   * `manifest.json` - what this package is, and the sha256 of every other file in it. A restore
//!     verifies these before writing anything, because a half-restored conversation is worse than a
//!     refused one.
//!   * `session.json` - the record: title, owner, timestamps, state.
//!   * `runs.jsonl` - one run per line. This is the canonical history: a rebuilt session shows
//!     goal/answer turns made from these, so a restore that has the runs has the conversation.
//!   * `transcript.jsonl` - the exact messages, for reading and for audit. Kept even though the runs
//!     could regenerate an equivalent view, because "equivalent" and "what was actually said" are
//!     not the same thing and an archive is the place to keep the difference.
//!   * `graphs.jsonl` - the task graphs, with the per-step results.
//!   * `artifacts/<id>-<name>` - the bytes of every file the conversation attached or produced.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{
    AgentRun, ArtifactRecord, PrincipalRef, SessionMessage, SessionRecord, TaskGraphRecord,
};
use agentos_core::now_ms;
use agentos_storage::artifact::ArtifactStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::path::Path;

/// The package layout version. A reader refuses a version it does not know rather than guessing.
pub const ARCHIVE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveManifest {
    pub format_version: u32,
    pub session_id: String,
    pub title: String,
    #[serde(default)]
    pub owner: Option<PrincipalRef>,
    /// The node that wrote the package. Two nodes archiving the same session id is possible in
    /// principle (a fork), and this is what tells them apart.
    pub node_id: String,
    pub created_at: u64,
    pub archived_at: u64,
    pub runs: usize,
    pub messages: usize,
    pub artifacts: usize,
    /// sha256 per file inside the package.
    pub files: BTreeMap<String, String>,
}

impl ArchiveManifest {
    /// The name a restore gives the conversation, unless the caller names it.
    pub fn restored_title(&self) -> String {
        format!("{} (restored)", self.title)
    }
}

/// One package on disk, as a listing sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// The file name stem, which is what the API addresses a package by.
    pub id: String,
    pub path: String,
    pub bytes: u64,
    pub manifest: ArchiveManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedBundle {
    pub id: String,
    pub path: String,
    pub bytes: u64,
    pub manifest: ArchiveManifest,
}

/// Files inside a package, in the order they are written.
const SESSION_FILE: &str = "session.json";
const RUNS_FILE: &str = "runs.jsonl";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const GRAPHS_FILE: &str = "graphs.jsonl";
const ARTIFACTS_FILE: &str = "artifacts.jsonl";
const MANIFEST_FILE: &str = "manifest.json";
const ARTIFACT_PREFIX: &str = "artifacts/";

fn zip_error(context: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::internal(format!("{context}: {error}"))
}

fn sha256(bytes: &[u8]) -> String {
    agentos_storage::blob::sha256_hex(bytes)
}

fn json_line<T: Serialize>(value: &T) -> Result<String> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// A file name that cannot escape the artifacts directory of a package.
fn safe_name(id: &str, name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        format!("{ARTIFACT_PREFIX}{id}")
    } else {
        format!("{ARTIFACT_PREFIX}{id}-{cleaned}")
    }
}

/// Write one conversation into a package under `root`.
///
/// Returns the path written, so a caller can say where it went instead of "archived".
#[allow(clippy::too_many_arguments)]
pub async fn write_package(
    root: &Path,
    record: &SessionRecord,
    runs: &[AgentRun],
    transcript: &[SessionMessage],
    graphs: &[TaskGraphRecord],
    artifacts: &dyn ArtifactStore,
    node_id: &str,
) -> Result<ArchivedBundle> {
    let archived_at = now_ms();
    // One directory per owner: an archive root shared by several people stays readable.
    let owner_dir = root.join(sanitise_segment(
        &record.owner_label().replace('@', "-at-"),
    ));
    tokio::fs::create_dir_all(&owner_dir)
        .await
        .map_err(|error| RuntimeError::unavailable(format!("archive root is not writable: {error}")))?;
    let stem = format!("{}-{archived_at}", record.id.as_str());
    let path = owner_dir.join(format!("{stem}.zip"));

    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut buffer: Vec<u8> = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
        let method = zip::write::FileOptions::<'_, ()>::default()
            .compression_method(zip::CompressionMethod::Deflated);

        let session_json = serde_json::to_vec_pretty(record)?;
        files.insert(SESSION_FILE.to_string(), sha256(&session_json));
        writer.start_file(SESSION_FILE, method).map_err(|e| zip_error("session.json", e))?;
        writer.write_all(&session_json).map_err(|e| zip_error("session.json", e))?;

        let mut runs_text = String::new();
        for run in runs {
            runs_text.push_str(&json_line(run)?);
        }
        files.insert(RUNS_FILE.to_string(), sha256(runs_text.as_bytes()));
        writer.start_file(RUNS_FILE, method).map_err(|e| zip_error("runs.jsonl", e))?;
        writer.write_all(runs_text.as_bytes()).map_err(|e| zip_error("runs.jsonl", e))?;

        let mut transcript_text = String::new();
        for message in transcript {
            transcript_text.push_str(&json_line(message)?);
        }
        files.insert(TRANSCRIPT_FILE.to_string(), sha256(transcript_text.as_bytes()));
        writer
            .start_file(TRANSCRIPT_FILE, method)
            .map_err(|e| zip_error("transcript.jsonl", e))?;
        writer
            .write_all(transcript_text.as_bytes())
            .map_err(|e| zip_error("transcript.jsonl", e))?;

        let mut graphs_text = String::new();
        for graph in graphs {
            graphs_text.push_str(&json_line(graph)?);
        }
        files.insert(GRAPHS_FILE.to_string(), sha256(graphs_text.as_bytes()));
        writer.start_file(GRAPHS_FILE, method).map_err(|e| zip_error("graphs.jsonl", e))?;
        writer.write_all(graphs_text.as_bytes()).map_err(|e| zip_error("graphs.jsonl", e))?;

        // The bytes of everything the conversation attached or produced, plus the records that
        // describe them: a restore has to put back the name, the kind and the content type, not only
        // the payload.
        let mut artifact_count = 0usize;
        let mut artifact_index = String::new();
        for artifact in artifacts.list(&record.id, 10_000).await? {
            let Some(bytes) = artifacts.read(&artifact.id).await? else {
                // A record without bytes is a dangling reference. Skipped rather than fatal: the
                // manifest says how many were written, and a reader can see the difference.
                continue;
            };
            let name = safe_name(artifact.id.as_str(), &artifact.name);
            files.insert(name.clone(), sha256(&bytes));
            writer
                .start_file(name.clone(), method)
                .map_err(|e| zip_error(&name, e))?;
            writer.write_all(&bytes).map_err(|e| zip_error(&name, e))?;
            artifact_index.push_str(&json_line(&artifact)?);
            artifact_count += 1;
        }
        files.insert(ARTIFACTS_FILE.to_string(), sha256(artifact_index.as_bytes()));
        writer
            .start_file(ARTIFACTS_FILE, method)
            .map_err(|e| zip_error("artifacts.jsonl", e))?;
        writer
            .write_all(artifact_index.as_bytes())
            .map_err(|e| zip_error("artifacts.jsonl", e))?;

        let manifest = ArchiveManifest {
            format_version: ARCHIVE_FORMAT_VERSION,
            session_id: record.id.as_str().to_string(),
            title: record.title.clone(),
            owner: record.owner.clone(),
            node_id: node_id.to_string(),
            created_at: record.created_at,
            archived_at,
            runs: runs.len(),
            messages: transcript.len(),
            artifacts: artifact_count,
            files,
        };
        let manifest_json = serde_json::to_vec_pretty(&manifest)?;
        writer
            .start_file(MANIFEST_FILE, method)
            .map_err(|e| zip_error("manifest.json", e))?;
        writer
            .write_all(&manifest_json)
            .map_err(|e| zip_error("manifest.json", e))?;
        writer.finish().map_err(|e| zip_error("the package", e))?;

        // Written through a temporary file and renamed: a crash mid-write leaves a `.part` file,
        // never a truncated package that looks like a good one.
        let temporary = path.with_extension("zip.part");
        tokio::fs::write(&temporary, &buffer)
            .await
            .map_err(|error| RuntimeError::unavailable(format!("archive root is not writable: {error}")))?;
        tokio::fs::rename(&temporary, &path)
            .await
            .map_err(|error| RuntimeError::unavailable(format!("could not finalise the package: {error}")))?;

        return Ok(ArchivedBundle {
            id: stem,
            path: path.display().to_string(),
            bytes: buffer.len() as u64,
            manifest,
        });
    }
}

/// Keep a path segment to characters no filesystem argues about.
fn sanitise_segment(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    if cleaned.trim_matches('.').is_empty() {
        "unowned".to_string()
    } else {
        cleaned
    }
}

/// Every package under an archive root, newest first.
///
/// Reads only each `manifest.json`, not the whole file: a listing of a hundred archives should not
/// read a hundred conversations.
pub async fn list_packages(root: &Path) -> Result<Vec<ArchiveEntry>> {
    let mut entries: Vec<ArchiveEntry> = Vec::new();
    let mut owners = match tokio::fs::read_dir(root).await {
        Ok(dir) => dir,
        // An archive root that does not exist yet is an empty archive, not an error: nothing has
        // been archived on this node.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => {
            return Err(RuntimeError::unavailable(format!(
                "archive root {} cannot be read: {error}",
                root.display()
            )));
        }
    };
    while let Some(owner) = owners
        .next_entry()
        .await
        .map_err(|error| RuntimeError::internal(format!("archive root: {error}")))?
    {
        let owner_path = owner.path();
        if !owner_path.is_dir() {
            continue;
        }
        let mut files = tokio::fs::read_dir(&owner_path)
            .await
            .map_err(|error| RuntimeError::internal(format!("archive directory: {error}")))?;
        while let Some(file) = files
            .next_entry()
            .await
            .map_err(|error| RuntimeError::internal(format!("archive directory: {error}")))?
        {
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("zip") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            // A package whose manifest cannot be read is skipped rather than failing the listing:
            // one damaged file must not hide the other ninety-nine.
            let Ok(manifest) = read_manifest(&path).await else {
                continue;
            };
            let bytes = file.metadata().await.map(|meta| meta.len()).unwrap_or(0);
            entries.push(ArchiveEntry {
                id: stem.to_string(),
                path: path.display().to_string(),
                bytes,
                manifest,
            });
        }
    }
    entries.sort_by(|a, b| b.manifest.archived_at.cmp(&a.manifest.archived_at));
    Ok(entries)
}

/// Find one package by its id, anywhere under the root.
pub async fn find_package(root: &Path, id: &str) -> Result<ArchiveEntry> {
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        // An id is a name, not a path: anything else is an attempt to read outside the root.
        return Err(RuntimeError::invalid_input(format!("invalid archive id {id:?}")));
    }
    list_packages(root)
        .await?
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| RuntimeError::not_found(format!("archive {id} does not exist")))
}

/// The manifest of a package, without reading the rest of it.
pub async fn read_manifest(path: &Path) -> Result<ArchiveManifest> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| RuntimeError::not_found(format!("archive {}: {error}", path.display())))?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| zip_error("the package is not a zip archive", error))?;
    let mut file = archive
        .by_name(MANIFEST_FILE)
        .map_err(|error| zip_error("the package has no manifest.json", error))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| zip_error("manifest.json", error))?;
    let manifest: ArchiveManifest = serde_json::from_str(&text)?;
    if manifest.format_version != ARCHIVE_FORMAT_VERSION {
        return Err(RuntimeError::conflict(format!(
            "package format {} is not version {ARCHIVE_FORMAT_VERSION}: this runtime cannot read it",
            manifest.format_version
        ))
        .with_detail("format_version", manifest.format_version));
    }
    Ok(manifest)
}

/// Everything a restore needs, verified.
pub struct PackageContents {
    pub manifest: ArchiveManifest,
    pub record: SessionRecord,
    pub runs: Vec<AgentRun>,
    /// The messages exactly as they were written. A restore does not need them (the runs regenerate
    /// the conversation), but a preview and an audit do.
    pub messages: Vec<SessionMessage>,
    pub graphs: Vec<TaskGraphRecord>,
    /// Each artifact with its record and its bytes, keyed by the id it had in the source session.
    pub artifacts: Vec<(ArtifactRecord, Vec<u8>)>,
}

impl PackageContents {
    /// The first turns of what was said, for a preview that does not read a whole conversation.
    pub fn preview(&self, limit: usize) -> Vec<SessionMessage> {
        self.messages.iter().take(limit).cloned().collect()
    }
}

/// Read a package and check every file against the manifest.
///
/// Verification is not optional: a restore writes into the live store, and a package that was
/// truncated by a full disk or edited by hand would put a conversation that never happened into
/// somebody's history. The checksum is the only thing standing between those two.
pub async fn read_package(path: &Path) -> Result<PackageContents> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| RuntimeError::not_found(format!("archive {}: {error}", path.display())))?;
    let mut archive = zip::ZipArchive::new(Cursor::new(&bytes))
        .map_err(|error| zip_error("the package is not a zip archive", error))?;
    let manifest: ArchiveManifest = {
        let mut file = archive
            .by_name(MANIFEST_FILE)
            .map_err(|error| zip_error("the package has no manifest.json", error))?;
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(|error| zip_error("manifest.json", error))?;
        serde_json::from_str(&text)?
    };
    if manifest.format_version != ARCHIVE_FORMAT_VERSION {
        return Err(RuntimeError::conflict(format!(
            "package format {} is not version {ARCHIVE_FORMAT_VERSION}",
            manifest.format_version
        )));
    }

    // Read every file once, checking as we go.
    let mut contents: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (name, expected) in &manifest.files {
        let mut file = archive.by_name(name).map_err(|error| {
            RuntimeError::invalid_input(format!("the package is missing {name}: {error}"))
        })?;
        let mut data = Vec::new();
        file.read_to_end(&mut data).map_err(|error| zip_error(name, error))?;
        let actual = sha256(&data);
        if &actual != expected {
            return Err(RuntimeError::invalid_input(format!(
                "{name} does not match its checksum: the package is damaged"
            ))
            .with_detail("file", name.clone()));
        }
        contents.insert(name.clone(), data);
    }

    let text_of = |name: &str| -> Result<String> {
        let data = contents
            .get(name)
            .ok_or_else(|| RuntimeError::invalid_input(format!("the package has no {name}")))?;
        Ok(String::from_utf8_lossy(data).to_string())
    };
    let record: SessionRecord = serde_json::from_str(&text_of(SESSION_FILE)?)?;
    let runs: Vec<AgentRun> = text_of(RUNS_FILE)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<AgentRun>)
        .collect::<std::result::Result<_, _>>()?;
    let messages: Vec<SessionMessage> = text_of(TRANSCRIPT_FILE)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<SessionMessage>)
        .collect::<std::result::Result<_, _>>()?;
    let graphs: Vec<TaskGraphRecord> = text_of(GRAPHS_FILE)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<TaskGraphRecord>)
        .collect::<std::result::Result<_, _>>()?;
    // Bytes by file name, then joined with the records that describe them.
    let mut payloads = BTreeMap::new();
    for (name, data) in &contents {
        if name.starts_with(ARTIFACT_PREFIX) {
            payloads.insert(name.clone(), data.clone());
        }
    }
    let records: Vec<ArtifactRecord> = text_of(ARTIFACTS_FILE)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<ArtifactRecord>)
        .collect::<std::result::Result<_, _>>()?;
    let mut artifacts = Vec::new();
    for artifact in records {
        let name = safe_name(artifact.id.as_str(), &artifact.name);
        let Some(bytes) = payloads.remove(&name) else {
            // The index names a file the package does not carry. Nothing was verified for it, so it
            // is refused rather than restored as an empty file.
            return Err(RuntimeError::invalid_input(format!(
                "the package lists artifact {} but does not carry {name}",
                artifact.id
            )));
        };
        artifacts.push((artifact, bytes));
    }
    Ok(PackageContents { manifest, record, runs, messages, graphs, artifacts })
}
