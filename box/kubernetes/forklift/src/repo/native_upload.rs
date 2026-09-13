//! Adapts the ecosystem-native publish protocols (npm's attachment envelope,
//! twine's legacy form) onto the managed upload contract, so a protocol publish
//! and a browser upload commit through exactly the same validated, atomic path.
//!
//! Both adapters re-encode the request as the multipart body [`Uploader::receive`] expects.

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Request};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::request::Parts;
use tokio::io::{AsyncWrite, AsyncWriteExt as _};

use crate::meta::{self, Repository};
use crate::server::http_error;

use super::npm::base64_decode_stream;
use super::router::Resolved;
use super::uiupload::{
    ArtifactUploadAsset, ArtifactUploadManifest, ArtifactUploadResult, NPMUploadManifest,
    PyPIUploadManifest, UploadProblem, random_upload_id,
};
use super::{Manager, path_base};

/// The principal a protocol publish is attributed to. Unauthenticated protocol
/// clients (an open hosted repository) publish as `native-client`.
fn native_upload_principal(parts: &Parts) -> (String, String) {
    match crate::auth::from_request_parts(parts) {
        Some(principal) => (principal.username.clone(), principal.source.clone()),
        None => ("native-client".to_string(), "protocol".to_string()),
    }
}

impl Manager {
    /// Hands a re-encoded native publish to the managed uploader under a fresh
    /// idempotency key (the protocol carries none).
    async fn receive_native_upload(
        &self,
        parts: &Parts,
        repository: &Repository,
        content_type: &str,
        body: Body,
    ) -> Result<ArtifactUploadResult, Box<UploadProblem>> {
        let unavailable = || {
            super::uiupload::upload_problem(
                503,
                "upload_id_failed",
                "Upload unavailable",
                "Could not allocate an upload identifier",
            )
        };
        let uploader = self.uploader.read().clone();
        let (Some(uploader), Some(idempotency_key)) = (uploader, random_upload_id()) else {
            return Err(unavailable());
        };
        let (name, source) = native_upload_principal(parts);
        let outcome = uploader
            .receive(
                repository,
                &name,
                &source,
                &idempotency_key,
                content_type,
                body,
            )
            .await;
        match outcome.problem {
            Some(problem) => Err(Box::new(problem)),
            None => Ok(outcome.result),
        }
    }

    /// Converts npm's base64 attachment envelope into the common streaming
    /// multipart contract. The common service validates `package.json` and
    /// atomically commits the tarball, packument, publication and upload record.
    pub(crate) async fn npm_publish_atomic(
        &self,
        parts: &Parts,
        res: &Resolved,
        doc: serde_json::Map<String, serde_json::Value>,
    ) -> Response {
        let Some(attachments) = doc.get("_attachments").and_then(|v| v.as_object()) else {
            return http_error(
                StatusCode::BAD_REQUEST,
                "publish document must contain exactly one attachment",
            );
        };
        if attachments.len() != 1 {
            return http_error(
                StatusCode::BAD_REQUEST,
                "publish document must contain exactly one attachment",
            );
        }
        let (filename, attachment) = attachments.iter().next().expect("one attachment");
        let Some(attachment) = attachment.as_object() else {
            return http_error(StatusCode::BAD_REQUEST, "invalid attachment");
        };
        let encoded = attachment
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if filename.is_empty() || encoded.is_empty() {
            return http_error(StatusCode::BAD_REQUEST, "attachment data is required");
        }
        let mut tag = "latest".to_string();
        if let Some(tags) = doc.get("dist-tags").and_then(|v| v.as_object())
            && !tags.contains_key("latest")
            && let Some(candidate) = tags.keys().next()
        {
            tag = candidate.clone();
        }
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_NPM.to_string(),
            assets: vec![ArtifactUploadAsset {
                part: "asset0".to_string(),
                role: "package".to_string(),
                ..Default::default()
            }],
            npm: Some(NPMUploadManifest { dist_tag: tag }),
            ..Default::default()
        };
        let Ok(tarball) = base64_decode_stream(encoded) else {
            return http_error(StatusCode::BAD_REQUEST, "invalid npm attachment");
        };
        // npm clients name the attachment `<name>-<version>.tgz`, so a scoped
        // package arrives as `@scope/name-1.0.0.tgz`. The multipart contract
        // refuses filenames with a path separator, so only the basename is
        // forwarded. Go's multipart reader did this implicitly.
        let filename = path_base(filename).to_string();
        let (boundary, content_type) = native_multipart_boundary();
        let (writer, reader) = tokio::io::duplex(64 * 1024);
        let write = async move {
            let mut writer = writer;
            write_manifest_part(&mut writer, &boundary, &manifest).await?;
            write_file_part(&mut writer, &boundary, "asset0", &filename, &tarball).await?;
            write_closing_boundary(&mut writer, &boundary).await
        };
        let receive = self.receive_native_upload(
            parts,
            &res.repo,
            &content_type,
            Body::from_stream(tokio_util::io::ReaderStream::new(reader)),
        );
        let (_, outcome) = tokio::join!(write, receive);
        match outcome {
            Err(problem) => native_upload_problem(&problem),
            Ok(_) => StatusCode::CREATED.into_response(),
        }
    }

    /// Adapts twine's legacy multipart form. Package identity is deliberately
    /// derived from the wheel/sdist metadata, not from the form fields.
    pub(crate) async fn pypi_upload_atomic(
        &self,
        parts: &Parts,
        res: &Resolved,
        body: Body,
    ) -> Response {
        let request = Request::from_parts(parts.clone(), body);
        let Ok(mut source) = Multipart::from_request(request, &()).await else {
            return http_error(StatusCode::BAD_REQUEST, "invalid multipart form");
        };
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_PYPI.to_string(),
            assets: vec![ArtifactUploadAsset {
                part: "asset0".to_string(),
                role: "distribution".to_string(),
                ..Default::default()
            }],
            pypi: Some(PyPIUploadManifest {}),
            ..Default::default()
        };
        let (boundary, content_type) = native_multipart_boundary();
        let (writer, reader) = tokio::io::duplex(64 * 1024);
        // Forwards the single `content` part and drops the metadata fields twine
        // sends alongside it; an error here abandons the body, which the
        // receiver reports as an invalid multipart upload.
        let write = async move {
            let mut writer = writer;
            write_manifest_part(&mut writer, &boundary, &manifest).await?;
            let mut found = false;
            while let Ok(Some(mut field)) = source.next_field().await {
                if field.name() != Some("content") {
                    let mut seen = 0i64;
                    while let Ok(Some(chunk)) = field.chunk().await {
                        seen += chunk.len() as i64;
                        if seen >= 1 << 20 {
                            break;
                        }
                    }
                    continue;
                }
                if found {
                    return Err(std::io::Error::other("duplicate content field"));
                }
                found = true;
                let filename = field.file_name().unwrap_or("").to_string();
                write_part_header(&mut writer, &boundary, "asset0", Some(&filename)).await?;
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|err| std::io::Error::other(err.to_string()))?
                {
                    writer.write_all(&chunk).await?;
                }
                writer.write_all(b"\r\n").await?;
            }
            if !found {
                return Err(std::io::Error::other("missing content field"));
            }
            write_closing_boundary(&mut writer, &boundary).await
        };
        let receive = self.receive_native_upload(
            parts,
            &res.repo,
            &content_type,
            Body::from_stream(tokio_util::io::ReaderStream::new(reader)),
        );
        let (_, outcome) = tokio::join!(write, receive);
        match outcome {
            Err(problem) => native_upload_problem(&problem),
            Ok(_) => StatusCode::CREATED.into_response(),
        }
    }
}

/// Renders an upload problem the way the native protocols report errors: the
/// detail as plain text under the problem's own status.
fn native_upload_problem(problem: &UploadProblem) -> Response {
    let status =
        StatusCode::from_u16(problem.status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    http_error(status, &problem.detail)
}

/// A random multipart boundary with the content type that announces it.
fn native_multipart_boundary() -> (String, String) {
    let boundary = random_upload_id().unwrap_or_else(|| "forkliftnativeupload".to_string());
    let content_type = format!("multipart/form-data; boundary={boundary}");
    (boundary, content_type)
}

/// Writes the `manifest` field, which every managed upload carries first.
async fn write_manifest_part<W: AsyncWrite + Unpin>(
    writer: &mut W,
    boundary: &str,
    manifest: &ArtifactUploadManifest,
) -> std::io::Result<()> {
    write_part_header(writer, boundary, "manifest", None).await?;
    let mut value = serde_json::to_vec(manifest).map_err(std::io::Error::other)?;
    value.push(b'\n');
    writer.write_all(&value).await?;
    writer.write_all(b"\r\n").await
}

/// Writes one whole file part.
async fn write_file_part<W: AsyncWrite + Unpin>(
    writer: &mut W,
    boundary: &str,
    name: &str,
    filename: &str,
    value: &[u8],
) -> std::io::Result<()> {
    write_part_header(writer, boundary, name, Some(filename)).await?;
    writer.write_all(value).await?;
    writer.write_all(b"\r\n").await
}

/// Writes a part's boundary and headers.
async fn write_part_header<W: AsyncWrite + Unpin>(
    writer: &mut W,
    boundary: &str,
    name: &str,
    filename: Option<&str>,
) -> std::io::Result<()> {
    let mut header = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"",
        escape_quotes(name)
    );
    if let Some(filename) = filename {
        header.push_str(&format!("; filename=\"{}\"", escape_quotes(filename)));
        header.push_str("\r\nContent-Type: application/octet-stream");
    }
    header.push_str("\r\n\r\n");
    writer.write_all(header.as_bytes()).await
}

async fn write_closing_boundary<W: AsyncWrite + Unpin>(
    writer: &mut W,
    boundary: &str,
) -> std::io::Result<()> {
    writer
        .write_all(format!("--{boundary}--\r\n").as_bytes())
        .await
}

fn escape_quotes(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
