use axum::{
    extract::{Json, Path as AxumPath, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Component, Path as StdPath, PathBuf};

// 🔒 Whitelist — only these directories are editable via the API.
// Add more if needed, but never include `src/`, `.env`, `Cargo.toml`, etc.
const ALLOWED_DIRS: &[&str] = &[
    "email_templates",
    "pages",
    "components",
    "layouts",
    "partials",
    "public",
];

// 🔒 Extension whitelist — everything else is rejected.
const ALLOWED_EXTS: &[&str] = &[
    "html", "vlo", "css", "js", "md", "txt", "json", "svg", "csv",
];

fn require_admin(auth: &crate::auth::AuthUser) -> Result<(), Response> {
    let Some(user) = auth.user.as_ref() else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Login required"}))).into_response());
    };
    if !user.role.eq_ignore_ascii_case("admin") {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": "Admin only"}))).into_response());
    }
    Ok(())
}

/// Validate a path and resolve it against the project root.
/// Returns None if the path escapes the whitelist or has a bad extension.
fn safe_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim_start_matches('/');
    if raw.is_empty() || raw.contains('\0') { return None; }

    let p = StdPath::new(raw);
    if p.is_absolute() { return None; }
    if p.components().any(|c| matches!(c, Component::ParentDir)) { return None; }

    let first = p.components().next()?.as_os_str().to_str()?;
    if !ALLOWED_DIRS.contains(&first) { return None; }

    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        if !ALLOWED_EXTS.iter().any(|x| x.eq_ignore_ascii_case(ext)) { return None; }
    }

    Some(crate::state::get_project_root().join(p))
}

// ═══════════════════════════════════════════════════════ LIST
#[derive(Deserialize)]
pub struct ListQuery { pub prefix: Option<String> }

pub async fn list_files(
    Extension(auth): Extension<crate::auth::AuthUser>,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Err(r) = require_admin(&auth) { return r; }

    let root = crate::state::get_project_root();
    let prefix = q.prefix.unwrap_or_default();

    let mut out: Vec<Value> = Vec::new();
    for dir in ALLOWED_DIRS {
        if !prefix.is_empty() && !dir.starts_with(&prefix) && !prefix.starts_with(dir) {
            continue;
        }
        let dir_path = root.join(dir);
        if !dir_path.exists() { continue; }
        walk(&dir_path, &root, &mut out);
    }
    out.sort_by(|a, b| {
        let ap = a.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let bp = b.get("path").and_then(|v| v.as_str()).unwrap_or("");
        ap.cmp(bp)
    });

    Json(json!({ "success": true, "data": out })).into_response()
}

fn walk(dir: &StdPath, root: &StdPath, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let rel = path.strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();

        if meta.is_dir() {
            out.push(json!({ "type": "dir", "path": rel }));
            walk(&path, root, out);
        } else if meta.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if !ALLOWED_EXTS.iter().any(|x| x.eq_ignore_ascii_case(ext)) { continue; }
            }
            let modified = meta.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()).unwrap_or(0);
            out.push(json!({
                "type": "file",
                "path": rel,
                "size": meta.len(),
                "modified": modified
            }));
        }
    }
}

// ═══════════════════════════════════════════════════════ READ
pub async fn read_file(
    Extension(auth): Extension<crate::auth::AuthUser>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&auth) { return r; }

    let Some(full) = safe_path(&path) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Invalid path"}))).into_response();
    };
    match std::fs::read_to_string(&full) {
        Ok(content) => Json(json!({
            "success": true,
            "data": [{ "path": path, "content": content }]
        })).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "Not found"}))).into_response(),
    }
}

// ═══════════════════════════════════════════════════════ WRITE
#[derive(Deserialize)]
pub struct WriteBody { pub content: String }

pub async fn write_file(
    Extension(auth): Extension<crate::auth::AuthUser>,
    AxumPath(path): AxumPath<String>,
    Json(body): Json<WriteBody>,
) -> Response {
    if let Err(r) = require_admin(&auth) { return r; }

    let Some(full) = safe_path(&path) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Invalid path"}))).into_response();
    };
    if body.content.len() > 1024 * 1024 {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": "Max 1MB"}))).into_response();
    }
    if let Some(parent) = full.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&full, &body.content) {
        Ok(_) => Json(json!({ "success": true, "path": path })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

// ═══════════════════════════════════════════════════════ DELETE
pub async fn delete_file(
    Extension(auth): Extension<crate::auth::AuthUser>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&auth) { return r; }

    let Some(full) = safe_path(&path) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Invalid path"}))).into_response();
    };
    match std::fs::remove_file(&full) {
        Ok(_) => Json(json!({ "success": true })).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "Not found"}))).into_response(),
    }
}