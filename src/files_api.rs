use axum::{
    body::Body,
    extract::{Multipart, Path as AxumPath},
    http::{
        header,
        HeaderValue,
        StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::fs as tokio_fs;

use crate::files;

//const DEFAULT_MAX_UPLOAD_MB: crate::state::max_upload_bytes();

pub async fn upload_file(mut multipart: Multipart) -> impl IntoResponse {
    if let Err(error) = files::ensure_storage() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "error": format!("Could not initialize storage: {}", error)
            })),
        )
            .into_response();
    }

    let max_bytes = max_upload_bytes();
    let mut uploaded = Vec::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "error": format!("Invalid multipart request: {}", error)
                    })),
                )
                    .into_response();
            }
        };

        let original_name = field
            .file_name()
            .map(sanitize_filename)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "upload".to_string());

        let mime_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();

        let data = match field.bytes().await {
            Ok(data) => data,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "error": format!("Could not read uploaded file: {}", error)
                    })),
                )
                    .into_response();
            }
        };

        if data.len() as u64 > max_bytes {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "success": false,
                    "error": format!(
                        "File exceeds maximum upload size of {} MB",
                        max_bytes / 1024 / 1024
                    )
                })),
            )
                .into_response();
        }

        let extension = Path::new(&original_name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(sanitize_extension)
            .filter(|ext| !ext.is_empty());

        let stored_name = generate_stored_name(extension.as_deref());

        let relative_path = stored_name.clone();

     let save_path = relative_path.clone();
     let save_data = data.clone();
     
     let save_result = tokio::task::spawn_blocking(move || {
         files::save(&save_path, &save_data)
     }).await;

     match save_result {
         Ok(Ok(_)) => {}
         Ok(Err(error)) => {
             return (
                 StatusCode::INTERNAL_SERVER_ERROR,
                 Json(json!({
                     "success": false,
                     "error": format!("Could not save file: {}", error)
                 })),
             ).into_response();
         }
         Err(join_error) => {
             return (
                 StatusCode::INTERNAL_SERVER_ERROR,
                 Json(json!({
                     "success": false,
                     "error": format!("File save task failed: {}", join_error)
                 })),
             ).into_response();
         }
     }

        uploaded.push(json!({
            "id": stored_name,
            "original_name": original_name,
            "stored_name": stored_name,
            "path": relative_path,
            "url": format!("/uploads/{}", url_path(&relative_path)),
            "download_url": format!("/api/files/{}/download", relative_path),
            "mime_type": mime_type,
            "size": data.len()
        }));
    }

    if uploaded.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "error": "No files were uploaded"
            })),
        )
            .into_response();
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "success": true,
            "files": uploaded
        })),
    )
        .into_response()
}

pub async fn serve_file(
    AxumPath(path): AxumPath<String>,
) -> Response {
    let Some(relative_path) = safe_relative_path(&path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let Some(full_path) = files::upload_path(&relative_path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let metadata = match tokio_fs::metadata(&full_path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let data = match tokio_fs::read(&full_path).await {
        Ok(data) => data,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let mime = mime_from_path(&full_path);

    let mut response = Response::new(Body::from(data));

    *response.status_mut() = StatusCode::OK;

    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime),
    );

    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );

    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );

    response
}

pub async fn get_file(
    AxumPath(id): AxumPath<String>,
) -> Response {
    serve_file_by_id(&id, false).await
}

pub async fn download_file(
    AxumPath(id): AxumPath<String>,
) -> Response {
    serve_file_by_id(&id, true).await
}

pub async fn delete_file(
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    let Some(relative_path) = safe_relative_path(&id) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "error": "Invalid file path"
            })),
        );
    };

    if !files::exists(&relative_path) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "success": false,
                "error": "File not found"
            })),
        );
    }

 let delete_path = relative_path.clone();
 let delete_result = tokio::task::spawn_blocking(move || {
     files::delete(&delete_path)
 }).await;

 match delete_result {
     Ok(Ok(_)) => (
         StatusCode::OK,
         Json(json!({
             "success": true,
             "id": relative_path
         })),
     ),
     Ok(Err(error)) => (
         StatusCode::INTERNAL_SERVER_ERROR,
         Json(json!({
             "success": false,
             "error": format!("Could not delete file: {}", error)
         })),
     ),
     Err(join_error) => (
         StatusCode::INTERNAL_SERVER_ERROR,
         Json(json!({
             "success": false,
             "error": format!("File delete task failed: {}", join_error)
         })),
     ),
 }
}

async fn serve_file_by_id(id: &str, download: bool) -> Response {
    let Some(relative_path) = safe_relative_path(id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let Some(full_path) = files::upload_path(&relative_path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let metadata = match tokio_fs::metadata(&full_path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let data = match tokio_fs::read(&full_path).await {
        Ok(data) => data,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let mime = mime_from_path(&full_path);

    let mut response = Response::new(Body::from(data));

    *response.status_mut() = StatusCode::OK;

    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime),
    );

    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );

    let disposition = if download {
        format!(
            "attachment; filename=\"{}\"",
            full_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("download")
        )
    } else {
        "inline".to_string()
    };

    if let Ok(value) = HeaderValue::from_str(&disposition) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }

    response
}

fn safe_relative_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");

    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\0')
        || path.split('/').any(|part| part == "..")
    {
        return None;
    }

    let candidate = Path::new(&path);

    if candidate.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return None;
    }

    Some(path)
}

fn sanitize_filename(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric()
                || matches!(ch, '.' | '-' | '_' | ' ')
            {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn sanitize_extension(extension: &str) -> String {
    extension
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(16)
        .collect()
}

fn generate_stored_name(extension: Option<&str>) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let process_id = std::process::id();

    match extension {
        Some(extension) if !extension.is_empty() => {
            format!("vlo_{timestamp}_{process_id}.{extension}")
        }
        _ => format!("vlo_{timestamp}_{process_id}"),
    }
}

fn max_upload_bytes() -> u64 {
    let mb = std::env::var("VLO_MAX_UPLOAD_MB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(crate::state::max_upload_bytes());

    mb.saturating_mul(1024 * 1024)
}

fn url_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn mime_from_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("csv") => "text/csv",
        Some("zip") => "application/zip",
        Some("doc") => "application/msword",
        Some("docx") => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => {
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        }
        _ => "application/octet-stream",
    }
}