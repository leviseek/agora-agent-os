//! Image attachments: read from the workspace, stored as artifacts, sent to the model.
//!
//! Two rules shape this module. First, an image is a file like any other, so it is read through
//! the same workspace jail - a path that escapes is refused, not sanitised. Second, the type of an
//! image is decided by its **magic bytes**, never by its name: a file called screenshot.png that
//! contains something else is refused, because the alternative is handing arbitrary bytes to a
//! model under a label we did not verify.

use agentos_capability_runtime::workspace::Workspace;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{ArtifactKind, ContentPart, SessionMessage};
use agentos_core::SessionId;
use agentos_model_router::ImageInput;
use agentos_storage::artifact::ArtifactStore;
use base64::Engine;
use std::sync::Arc;

/// The largest image we will read, store and send. Chosen to be generous for screenshots and
/// small for anything that would make a request pathological.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// The formats the runtime will pass on, decided by content.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // The RIFF header plus the WEBP tag is exactly twelve bytes.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// What was attached: the transcript parts, and the encoded images for the model call.
#[derive(Default)]
pub struct Attached {
    pub parts: Vec<ContentPart>,
    pub images: Vec<ImageInput>,
}

/// Read, verify and store the images a user attached to one message.
///
/// `paths` are workspace-relative files (the runtime reads them through the jail); `attachment_ids`
/// are artifacts an earlier upload already stored (a console cannot write into the runtime's
/// workspace, so the bytes arrive through the API instead). Both end up as the same kind of
/// artifact and the same kind of model input.
///
/// A path or an id that cannot be resolved is an error, not a silent omission: a user who attached
/// a screenshot and got an answer about nothing would have no way to tell why.
pub async fn attach_images(
    workspace: &Workspace,
    artifacts: &Arc<dyn ArtifactStore>,
    session: &SessionId,
    paths: &[String],
    attachment_ids: &[String],
) -> Result<Attached> {
    let mut attached = attach_uploaded(artifacts, session, attachment_ids).await?;
    let from_paths = attach_paths(workspace, artifacts, session, paths).await?;
    attached.parts.extend(from_paths.parts);
    attached.images.extend(from_paths.images);
    Ok(attached)
}

/// Store bytes a client uploaded. The type is decided by content here too: the caller's declared
/// mime is a hint, never the decision.
pub async fn store_upload(
    artifacts: &Arc<dyn ArtifactStore>,
    session: &SessionId,
    name: &str,
    declared_mime: Option<&str>,
    bytes: &[u8],
) -> Result<agentos_core::model::ArtifactRecord> {
    if bytes.is_empty() {
        return Err(RuntimeError::invalid_input("the uploaded image is empty"));
    }
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(RuntimeError::invalid_input(format!(
            "the uploaded image is {} bytes, the limit is {MAX_IMAGE_BYTES}",
            bytes.len()
        )));
    }
    let mime = sniff_mime(bytes).ok_or_else(|| {
        RuntimeError::invalid_input(format!(
            "{} is not a PNG, JPEG, GIF or WebP image (the declared type {} was not trusted: the \
             type is decided by content)",
            name,
            declared_mime.unwrap_or("none")
        ))
    })?;
    artifacts
        .put(session.clone(), name, ArtifactKind::Binary, mime, bytes)
        .await
}

/// Turn stored artifacts into model input, checking the bytes again: an artifact id is a claim,
/// and the claim is verified the same way a path is.
async fn attach_uploaded(
    artifacts: &Arc<dyn ArtifactStore>,
    session: &SessionId,
    attachment_ids: &[String],
) -> Result<Attached> {
    let mut attached = Attached::default();
    for id in attachment_ids {
        let artifact_id = agentos_core::ArtifactId::from_raw(id.clone());
        let record = artifacts.get(&artifact_id).await?.ok_or_else(|| {
            RuntimeError::not_found(format!("no uploaded attachment with id {id}"))
        })?;
        let bytes = artifacts.read(&artifact_id).await?.ok_or_else(|| {
            RuntimeError::not_found(format!("attachment {id} has no bytes"))
        })?;
        let mime = sniff_mime(&bytes).ok_or_else(|| {
            RuntimeError::invalid_input(format!(
                "attachment {id} ({}) is not a PNG, JPEG, GIF or WebP image",
                record.name
            ))
        })?;
        attached.parts.push(ContentPart::Image {
            artifact_id: id.clone(),
            name: record.name.clone(),
            mime: mime.to_string(),
        });
        attached.images.push(ImageInput {
            mime: mime.to_string(),
            base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    let _ = session;
    Ok(attached)
}

async fn attach_paths(
    workspace: &Workspace,
    artifacts: &Arc<dyn ArtifactStore>,
    session: &SessionId,
    paths: &[String],
) -> Result<Attached> {
    let mut attached = Attached::default();
    for path in paths {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_string();
        let resolved = workspace.resolve(path)?;
        let meta = tokio::fs::metadata(&resolved).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                RuntimeError::not_found(format!("image not found in the workspace: {path}"))
            } else {
                RuntimeError::invalid_input(format!("cannot read image {path}: {error}"))
            }
        })?;
        if !meta.is_file() {
            return Err(RuntimeError::invalid_input(format!("not a file: {path}")));
        }
        if meta.len() > MAX_IMAGE_BYTES {
            return Err(RuntimeError::invalid_input(format!(
                "image {path} is {} bytes, the limit is {MAX_IMAGE_BYTES}",
                meta.len()
            )));
        }
        let bytes = tokio::fs::read(&resolved).await?;
        let mime = sniff_mime(&bytes).ok_or_else(|| {
            RuntimeError::invalid_input(format!(
                "{path} is not a PNG, JPEG, GIF or WebP image (the type is decided by content, not by the file name)"
            ))
        })?;

        let record = artifacts
            .put(session.clone(), &name, ArtifactKind::Binary, mime, &bytes)
            .await?;
        attached.parts.push(ContentPart::Image {
            artifact_id: record.id.as_str().to_string(),
            name: name.clone(),
            mime: mime.to_string(),
        });
        attached.images.push(ImageInput {
            mime: mime.to_string(),
            base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    Ok(attached)
}

/// The user message for a goal that may carry images.
pub fn user_message(session: &SessionId, text: &str, images: &[ContentPart]) -> SessionMessage {
    let mut message = SessionMessage::user(session.clone(), text);
    message.parts.extend(images.iter().cloned());
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_type_comes_from_the_content() {
        let png = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert_eq!(sniff_mime(&png), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a..."), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff_mime(&webp), Some("image/webp"));
        // A text file, whatever somebody called it.
        assert_eq!(sniff_mime(b"not really an image"), None);
        assert_eq!(sniff_mime(b""), None);
    }
}