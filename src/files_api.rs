use axum::{
    body::Body,
    extract::{Multipart, Path as AxumPath, Query},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use image::ImageEncoder;
use serde::Deserialize;
use serde_json::json;
use std::{
    io::Cursor,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::fs as tokio_fs;

use crate::files;
use crate::state::{get_project_root, max_upload_bytes};

// ===========================================================================
// 1. UPLOAD (unchanged)
// ===========================================================================
pub async fn upload_file(mut multipart: Multipart) -> impl IntoResponse {
    if let Err(error) = files::ensure_storage() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "error": format!("Could not initialize storage: {}", error)
            })),
        ).into_response();
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
                ).into_response();
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
                ).into_response();
            }
        };

        if data.len() as u64 > max_bytes {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "success": false,
                    "error": format!("File exceeds maximum upload size of {} MB", max_bytes / 1024 / 1024)
                })),
            ).into_response();
        }

        let extension = Path::new(&original_name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(sanitize_extension)
            .filter(|ext| !ext.is_empty());

        let stored_name = generate_stored_name(extension.as_deref());
        let save_path = stored_name.clone();
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
            "stored_name": stored_name.clone(),
            "path": stored_name.clone(),
            "url": format!("/storage/uploads/{}", url_path(&stored_name)),
            "download_url": format!("/api/files/{}/download", stored_name),
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
        ).into_response();
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "success": true,
            "files": uploaded
        })),
    ).into_response()
}

// ===========================================================================
// 2. SERVE & DOWNLOAD
// ===========================================================================
pub async fn serve_file(AxumPath(path): AxumPath<String>) -> Response {
    serve_file_by_id(&path, false).await
}

pub async fn get_file(AxumPath(id): AxumPath<String>) -> Response {
    serve_file_by_id(&id, false).await
}

pub async fn download_file(AxumPath(id): AxumPath<String>) -> Response {
    serve_file_by_id(&id, true).await
}

async fn serve_file_by_id(id: &str, download: bool) -> Response {
    // Strip optional leading prefixes so bare filenames work too.
    let bare = id
        .trim_start_matches('/')
        .trim_start_matches("storage/uploads/")
        .trim_start_matches("uploads/");

    let Some(relative_path) = safe_relative_path(bare) else {
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

    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );

    let disposition = if download {
        format!(
            "attachment; filename=\"{}\"",
            full_path.file_name().and_then(|n| n.to_str()).unwrap_or("download")
        )
    } else {
        "inline".to_string()
    };

    if let Ok(value) = HeaderValue::from_str(&disposition) {
        response.headers_mut().insert(header::CONTENT_DISPOSITION, value);
    }

    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );

    response
}

// ===========================================================================
// 3. DELETE
// ===========================================================================
pub async fn delete_file(AxumPath(id): AxumPath<String>) -> impl IntoResponse {
    let bare = id
        .trim_start_matches('/')
        .trim_start_matches("storage/uploads/")
        .trim_start_matches("uploads/");

    let Some(relative_path) = safe_relative_path(bare) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Invalid file path"})));
    };

    if !files::exists(&relative_path) {
        return (StatusCode::NOT_FOUND, Json(json!({"success": false, "error": "File not found"})));
    }

    let delete_path = relative_path.clone();
    let delete_result = tokio::task::spawn_blocking(move || files::delete(&delete_path)).await;

    match delete_result {
        Ok(Ok(_)) => (StatusCode::OK, Json(json!({"success": true, "id": relative_path}))),
        Ok(Err(error)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": format!("Could not delete file: {}", error)}))),
        Err(join_error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": format!("File delete task failed: {}", join_error)}))),
    }
}

// ===========================================================================
// 4. IMAGE OPTIMIZATION
// ===========================================================================
#[derive(Deserialize)]
pub struct OptimizeQuery {
    src: String,
    w: Option<u32>,
    h: Option<u32>,
    f: Option<String>,
    q: Option<u8>,
    fit: Option<String>,
}

const MAX_IMAGE_DIM: u32 = 4096;
const MAX_IMAGE_QUALITY: u8 = 100;

pub async fn optimize_image(Query(params): Query<OptimizeQuery>) -> Response {
    // ── 1. Path validation ────────────────────────────────────────
    let clean_src = params.src.trim_start_matches('/');
    let normalized = clean_src.replace('\\', "/");

    if normalized.contains("..") {
        return (StatusCode::BAD_REQUEST, "Invalid image path").into_response();
    }

    // Accepts:
    //   /storage/uploads/xxx.png   (canonical, from modifier)
    //   /uploads/xxx.png           (legacy)
    //   uploads/xxx.png            (legacy)
    //   xxx.png                    (bare filename, new standard)
    let relative = if let Some(rest) = normalized.strip_prefix("storage/") {
        rest.to_string()
    } else if normalized.starts_with("uploads/") {
        normalized.clone()
    } else if normalized.contains('/') {
        return (StatusCode::BAD_REQUEST, "Invalid image path").into_response();
    } else {
        format!("uploads/{}", normalized)
    };

    if relative.split('/').any(|part| part == ".." || part.is_empty()) {
        return (StatusCode::BAD_REQUEST, "Invalid image path").into_response();
    }

    let full_path = get_project_root().join("storage").join(&relative);
    if !full_path.exists() || !full_path.is_file() {
        return (StatusCode::NOT_FOUND, "Image not found").into_response();
    }

    // ── 2. Parse + clamp options ──────────────────────────────────
    let target_format = params.f.as_deref().unwrap_or("webp").to_ascii_lowercase();
    let quality = params.q.unwrap_or(80).min(MAX_IMAGE_QUALITY);
    let width = params.w.filter(|w| *w > 0 && *w <= MAX_IMAGE_DIM);
    let height = params.h.filter(|h| *h > 0 && *h <= MAX_IMAGE_DIM);
    let fit = params.fit.as_deref().unwrap_or("exact");

    // ── 3. Disk cache fast path ───────────────────────────────────
    let cache_dir = get_project_root().join(".vlo").join("cache").join("images");
    let _ = std::fs::create_dir_all(&cache_dir);
    let cache_key = image_cache_key(&relative, width, height, &target_format, quality, fit);
    let cache_path = cache_dir.join(format!("{}.bin", cache_key));

    if let Ok(bytes) = tokio_fs::read(&cache_path).await {
        if !bytes.is_empty() {
            let ct = mime_for_format(&target_format);
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            return response;
        }
    }

    // ── 4. Process in blocking thread ─────────────────────────────
    let fit_owned = fit.to_string();
    let process_result = tokio::task::spawn_blocking(
        move || -> Result<(Vec<u8>, &'static str), StatusCode> {
            let img = image::open(&full_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            // Resize strategy:
            //   exact   → resize_exact (may stretch)
            //   cover   → resize_to_fill (crop overflow)
            //   contain → resize + letterbox on a transparent canvas
            let processed = match (width, height, fit_owned.as_str()) {
                (Some(w), Some(h), "cover") => {
                    img.resize_to_fill(w, h, image::imageops::FilterType::Lanczos3)
                }
                (Some(w), Some(h), "contain") => {
                    let fitted = img.resize(w, h, image::imageops::FilterType::Lanczos3);
                    let mut canvas = image::RgbaImage::new(w, h);
                    let x = (w.saturating_sub(fitted.width())) / 2;
                    let y = (h.saturating_sub(fitted.height())) / 2;
                    image::imageops::overlay(
                        &mut canvas,
                        &fitted.to_rgba8(),
                        x as i64,
                        y as i64,
                    );
                    image::DynamicImage::ImageRgba8(canvas)
                }
                (Some(w), Some(h), _) => {
                    img.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
                }
                (Some(w), None, _) => {
                    let ratio = w as f64 / img.width().max(1) as f64;
                    let h = ((img.height() as f64) * ratio) as u32;
                    img.resize_exact(w, h.max(1), image::imageops::FilterType::Lanczos3)
                }
                (None, Some(h), _) => {
                    let ratio = h as f64 / img.height().max(1) as f64;
                    let w = ((img.width() as f64) * ratio) as u32;
                    img.resize_exact(w.max(1), h, image::imageops::FilterType::Lanczos3)
                }
                (None, None, _) => img,
            };

            let mut output: Vec<u8> = Vec::with_capacity(64 * 1024);

            let content_type: &'static str = match target_format.as_str() {
                "jpg" | "jpeg" => {
                    let rgb = processed.to_rgb8();
                    let mut cursor = Cursor::new(&mut output);
                    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                        &mut cursor,
                        quality,
                    );
                    encoder
                        .encode(
                            rgb.as_raw(),
                            rgb.width(),
                            rgb.height(),
                            image::ExtendedColorType::Rgb8,
                        )
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    "image/jpeg"
                }
                "png" => {
                    let rgba = processed.to_rgba8();
                    let mut cursor = Cursor::new(&mut output);
                    image::codecs::png::PngEncoder::new(&mut cursor)
                        .write_image(
                            rgba.as_raw(),
                            rgba.width(),
                            rgba.height(),
                            image::ExtendedColorType::Rgba8,
                        )
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    "image/png"
                }
                // webp + avif fallback (no pure-Rust avif encoder in `image`)
                _ => {
                    let rgba = processed.to_rgba8();
                    let mut cursor = Cursor::new(&mut output);
                    image::codecs::webp::WebPEncoder::new_lossless(&mut cursor)
                        .encode(
                            rgba.as_raw(),
                            rgba.width(),
                            rgba.height(),
                            image::ExtendedColorType::Rgba8,
                        )
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    "image/webp"
                }
            };

            if output.is_empty() {
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            Ok((output, content_type))
        },
    )
    .await;

    // ── 5. Build response + write cache ───────────────────────────
    match process_result {
        Ok(Ok((bytes, ct))) => {
            let cache_path_clone = cache_path.clone();
            let bytes_for_cache = bytes.clone();
            tokio::spawn(async move {
                let _ = tokio_fs::write(&cache_path_clone, &bytes_for_cache).await;
            });

            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            response
        }
        Ok(Err(status)) => status.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// ===========================================================================
// 5. UTILITIES
// ===========================================================================
fn safe_relative_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    if path.is_empty() || path.starts_with('/') || path.contains('\0') || path.split('/').any(|part| part == "..") {
        return None;
    }
    let candidate = Path::new(&path);
    if candidate.components().any(|component| {
        matches!(component, std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_))
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
        .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') { ch } else { '_' })
        .collect()
}

fn sanitize_extension(extension: &str) -> String {
    extension.chars().filter(|ch| ch.is_ascii_alphanumeric()).take(16).collect()
}

fn generate_stored_name(extension: Option<&str>) -> String {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let process_id = std::process::id();
    match extension {
        Some(extension) if !extension.is_empty() => format!("vlo_{timestamp}_{process_id}.{extension}"),
        _ => format!("vlo_{timestamp}_{process_id}"),
    }
}

fn url_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn mime_from_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()).as_deref() {
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
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

// ── Image-specific helpers ───────────────────────────────────────
fn mime_for_format(fmt: &str) -> &'static str {
    match fmt {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        _ => "image/webp",
    }
}

fn image_cache_key(
    src: &str,
    w: Option<u32>,
    h: Option<u32>,
    fmt: &str,
    q: u8,
    fit: &str,
) -> String {
    // FNV-1a hash — no new dependencies.
    let mut hash: u64 = 0xcbf29ce484222325;
    let feed = |hash: &mut u64, bytes: &[u8]| {
        for &b in bytes {
            *hash ^= b as u64;
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    feed(&mut hash, src.as_bytes());
    feed(&mut hash, b"|");
    feed(&mut hash, &w.unwrap_or(0).to_le_bytes());
    feed(&mut hash, &h.unwrap_or(0).to_le_bytes());
    feed(&mut hash, b"|");
    feed(&mut hash, fmt.as_bytes());
    feed(&mut hash, b"|");
    feed(&mut hash, fit.as_bytes());
    feed(&mut hash, &[q]);
    format!("{:016x}", hash)
}