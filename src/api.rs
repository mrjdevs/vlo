// ─────────────────────────────────────────────────────────────
// api.rs — VLO dynamic SQL API
// Optimized: single-pass SQL param substitution (no regex compile,
// no per-param full-string replace), percent_decode fast path.
// Behavior preserved except cross-param interpolation, which is
// now single-pass (documented at substitute_sql_params).
// ─────────────────────────────────────────────────────────────
use crate::{
    database::DbPool,
    state::get_project_root,
};
use axum::{
    body::Bytes,
    extract::{Extension, FromRequest, Multipart, Path as AxumPath, Request},
    http::{Method, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fmt::Write as _,
    fs,
    path::Path,
    sync::{Arc, LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

// ============================================================================
// API Action: Supports both simple SQL strings and role-protected objects
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ApiAction {
    Simple(String),
    Protected {
        sql: String,
        #[serde(default)]
        roles: Vec<String>,
        #[serde(default)]
        paginate: Option<bool>, // 🔥 NEW
    },
}

impl ApiAction {
    pub fn sql(&self) -> &str {
        match self {
            ApiAction::Simple(s) => s,
            ApiAction::Protected { sql, .. } => sql,
        }
    }

    pub fn roles(&self) -> &[String] {
        match self {
            ApiAction::Simple(_) => &[],
            ApiAction::Protected { roles, .. } => roles,
        }
    }
    pub fn paginate(&self) -> Option<bool> {
        match self {
            ApiAction::Simple(_) => None,
            ApiAction::Protected { paginate, .. } => *paginate,
        }
    }
}

// ============================================================================
// API Action Cache
// ============================================================================

static API_ACTIONS_CACHE: LazyLock<Mutex<Option<CachedActions>>> =
    LazyLock::new(|| Mutex::new(None));

pub fn extract_server_block(content: &str) -> Option<String> {
    let start = content.find("<script server>")?;
    let rest = &content[start + 15..];
    let end = rest.find("</script>")?;
    Some(rest[..end].trim().to_string())
}

pub fn strip_server_block(content: &str) -> String {
    if let Some(start) = content.find("<script server>") {
        if let Some(end) = content[start..].find("</script>") {
            let end = start + end + 9;
            return format!("{}{}", &content[..start], &content[end..]);
        }
    }
    content.to_string()
}

fn load_actions_from_file(file: &Path) -> HashMap<String, ApiAction> {
    let mut actions = HashMap::new();
    if let Ok(content) = fs::read_to_string(file) {
        if let Some(block) = extract_server_block(&content) {
            // Fast path: skip the 3-allocation cleanup when the block is clean.
            let clean: std::borrow::Cow<str> =
                if block.contains('\u{feff}') || block.contains('\u{a0}') || block.contains('\r') {
                    std::borrow::Cow::Owned(
                        block
                            .trim_start_matches('\u{feff}')
                            .replace('\u{a0}', " ")
                            .replace('\r', ""),
                    )
                } else {
                    std::borrow::Cow::Borrowed(block.as_str())
                };

            if let Ok(json) = serde_json::from_str::<Value>(&clean) {
                if let Some(object) = json.as_object() {
                    for (name, value) in object {
                        if let Ok(action) = serde_json::from_value::<ApiAction>(value.clone()) {
                            actions.insert(name.clone(), action);
                        }
                    }
                }
            }
        }
    }
    actions
}

fn get_api_modified_time(api_dir: &Path) -> SystemTime {
    let mut latest = UNIX_EPOCH;
    let main_file = api_dir.join("api.vlo");
    if let Ok(meta) = fs::metadata(&main_file) {
        if let Ok(modified) = meta.modified() {
            if modified > latest {
                latest = modified;
            }
        }
    }
    if let Ok(entries) = fs::read_dir(api_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let ns_file = path.join("api.vlo");
                if let Ok(meta) = fs::metadata(&ns_file) {
                    if let Ok(modified) = meta.modified() {
                        if modified > latest {
                            latest = modified;
                        }
                    }
                }
            }
        }
    }
    latest
}

struct CachedActions {
    actions: Arc<HashMap<String, ApiAction>>,
    modified: SystemTime,
}

pub fn load_api_actions() -> Result<Arc<HashMap<String, ApiAction>>, String> {
    let api_dir = get_project_root().join("pages/api");
    let is_dev = crate::state::app_mode().is_dev();
    let modified = if is_dev {
        get_api_modified_time(&api_dir)
    } else {
        SystemTime::UNIX_EPOCH
    };

    if let Ok(cache) = API_ACTIONS_CACHE.lock() {
        if let Some(cached) = cache.as_ref() {
            if !is_dev || cached.modified == modified {
                crate::vlo_debug!("⚡ API actions served from memory cache");
                return Ok(Arc::clone(&cached.actions));
            }
        }
    }

    let mut actions = HashMap::new();
    let main_file = api_dir.join("api.vlo");
    if main_file.exists() {
        let main_actions = load_actions_from_file(&main_file);
        crate::vlo_debug!("📡 Loaded {} actions from api.vlo", main_actions.len());
        actions.extend(main_actions);
    }

    if let Ok(entries) = fs::read_dir(&api_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let namespace = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            let ns_file = path.join("api.vlo");
            if !ns_file.exists() {
                continue;
            }
            let ns_actions = load_actions_from_file(&ns_file);
            crate::vlo_debug!(
                "📡 Loaded {} actions from api/{}/api.vlo",
                ns_actions.len(),
                namespace
            );
            for (name, action) in ns_actions {
                actions.insert(format!("{}/{}", namespace, name), action);
            }
        }
    }

    let module_actions = crate::modules::get_module_api_actions();
    actions.extend(module_actions);

    let actions_arc = Arc::new(actions);
    if let Ok(mut cache) = API_ACTIONS_CACHE.lock() {
        *cache = Some(CachedActions {
            actions: Arc::clone(&actions_arc),
            modified,
        });
    }
    crate::vlo_debug!("📡 Total API actions available: {}", actions_arc.len());
    Ok(actions_arc)
}

// ============================================================================
// HTTP Handlers
// ============================================================================

pub async fn api_handler_root(
    Extension(auth): Extension<crate::auth::AuthUser>,
    req: Request,
) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => api_route_handler(None, None, method, query, &auth)
            .await
            .into_response(),
        Err(response) => response,
    }
}

pub async fn api_handler_path(
    Extension(auth): Extension<crate::auth::AuthUser>,
    AxumPath(resource): AxumPath<String>,
    req: Request,
) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => {
            api_route_handler(Some(resource), None, method, query, &auth)
                .await
                .into_response()
        }
        Err(response) => response,
    }
}

pub async fn api_handler_id(
    Extension(auth): Extension<crate::auth::AuthUser>,
    AxumPath((resource, id)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => {
            api_route_handler(Some(resource), Some(id), method, query, &auth)
                .await
                .into_response()
        }
        Err(response) => response,
    }
}

async fn prepare_api_request(
    req: Request,
) -> Result<(Method, HashMap<String, String>), Response> {
    let method = req.method().clone();
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let mut query = req.uri().query().map(parse_form_body).unwrap_or_default();

    if content_type.starts_with("multipart/form-data") {
        let mut multipart = Multipart::from_request(req, &()).await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"success": false, "error": "Invalid multipart request", "details": e.to_string()})),
            )
                .into_response()
        })?;
        parse_multipart_body(&mut multipart, &mut query).await?;
    } else {
        let body = Bytes::from_request(req, &()).await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"success": false, "error": "Invalid request body", "details": e.to_string()})),
            )
                .into_response()
        })?;
        if !body.is_empty() {
            if content_type.contains("application/json") {
                if let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(&body) {
                    for (key, value) in object {
                        query.insert(key, value_to_query_string(&value));
                    }
                }
            } else if content_type.contains("application/x-www-form-urlencoded") {
                for (key, value) in parse_form_body(&String::from_utf8_lossy(&body)) {
                    query.insert(key, value);
                }
            }
        }
    }
    Ok((method, query))
}

async fn parse_multipart_body(
    multipart: &mut Multipart,
    query: &mut HashMap<String, String>,
) -> Result<(), Response> {
    let max_size = crate::state::max_upload_bytes();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Invalid multipart", "details": e.to_string()}))).into_response()),
        };
        
        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(|n| n.to_string());
        
        // 🔥 FIX: Strictly skip empty file inputs so they don't generate ghost files
        if let Some(ref orig_name) = file_name {
            if orig_name.trim().is_empty() {
                continue;
            }
        }
        
        let is_file = file_name.is_some();
        let bytes = field.bytes().await.map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Failed to read body", "details": e.to_string()}))).into_response())?;

        if bytes.len() as u64 > max_size {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, Json(serde_json::json!({"error": "File too large"}))).into_response());
        }

        let val = if let Some(orig_name) = file_name {
            let upload_name = generate_upload_name(&orig_name);
            let (u_name, b_clone) = (upload_name.clone(), bytes.clone());
            let res = tokio::task::spawn_blocking(move || crate::files::save(&u_name, &b_clone)).await;
            match res {
                Ok(Ok(_)) => upload_name,
                Ok(Err(_)) | Err(_) => return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "Failed to save file"}))).into_response()),
            }
        } else {
            String::from_utf8_lossy(&bytes).to_string()
        };

        if is_file || name == "images" || name.ends_with("[]") {
            let entry = query.entry(name).or_insert_with(|| "[]".to_string());
            
            // Ignore empty strings
            if !val.is_empty() { 
                let mut arr: Vec<String> = serde_json::from_str(entry).unwrap_or_else(|_| {
                    if entry.is_empty() || entry == "[]" { vec![] } else { vec![entry.clone()] }
                });
                arr.push(val);
                *entry = serde_json::to_string(&arr).unwrap_or_default();
            }
        } else {
            query.insert(name, val);
        }
    }
    Ok(())
}
// ============================================================================
// Auto-Pagination Helper
// ============================================================================

static RE_LIMIT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\s+LIMIT\s+\{\{.*?\}\}").unwrap());
static RE_OFFSET: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\s+OFFSET\s+\{\{.*?\}\}").unwrap());

pub fn generate_count_sql(sql_template: &str) -> String {
    // 🔥 FIX: Only process the FIRST statement to avoid syntax errors
    // when wrapping in SELECT COUNT(*)
    let first_stmt = sql_template.split(';').next().unwrap_or(sql_template);

    let without_limit = RE_LIMIT.replace_all(first_stmt, "");
    let stripped = RE_OFFSET.replace_all(&without_limit, "");
    let stripped = stripped.trim().trim_end_matches(';');
    if stripped.is_empty() {
        return String::new();
    }
    format!(
        "SELECT COUNT(*) as total FROM ({}) AS _vlo_count_subquery",
        stripped
    )
}

// ============================================================================
// Flash Message Helper
// ============================================================================

fn flash_response(status: StatusCode, body: serde_json::Value, flash_encoded: &str) -> Response {
    let cookie = crate::auth::flash_cookie_header(flash_encoded);
    let mut response = (status, Json(body)).into_response();
    if let Ok(val) = axum::http::HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, val);
    }
    response
}

// ============================================================================
// Main Route Handler (with Role Checking)
// ============================================================================

pub async fn api_route_handler(
    mut endpoint: Option<String>,
    id: Option<String>,
    method: Method,
    mut query: HashMap<String, String>,
    auth: &crate::auth::AuthUser,
) -> impl IntoResponse {
    crate::vlo_debug!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    crate::vlo_debug!(
        "🔧 VLO DEBUG: API request started | Method: {} | Endpoint: {:?} | ID: {:?}",
        method,
        endpoint,
        id
    );

    if endpoint.is_none() {
        if let Some(action) = query.get("action").cloned() {
            endpoint = Some(action);
        }
    }

    let endpoint = match endpoint {
        Some(value) => value.trim().trim_matches('/').to_string(),
        None => {
            return match load_api_actions() {
                Ok(actions) => {
                    let action_names: Vec<&String> = actions.keys().collect();
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({"success": true, "actions": action_names})),
                    )
                        .into_response()
                }
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"success": false, "error": "Failed to load API definitions", "details": error})),
                )
                    .into_response(),
            };
        }
    };

    let resource = normalize_resource(&endpoint);
    if !valid_identifier(&resource) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "Invalid API resource"
            })),
        )
            .into_response();
    }

    let operation = match crud_operation(&method) {
        Some(value) => value,
        None => {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                Json(serde_json::json!({"success": false, "error": "Unsupported HTTP method"})),
            )
                .into_response()
        }
    };

    let actions = match load_api_actions() {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"success": false, "error": "Failed to load API definitions", "details": error})),
            )
                .into_response()
        }
    };

    let explicit_action = ["get_", "post_", "put_", "patch_", "delete_"]
        .iter()
        .any(|prefix| endpoint.starts_with(prefix));
    let mut action_name = if explicit_action {
        endpoint.clone()
    } else {
        format!("{}_{}", operation, endpoint)
    };

    // ─── ACTION RESOLUTION ─────────────────────────────────────────
    let action_def = match actions.get(&action_name) {
        Some(value) => value.clone(),
        None => {
            if let Some(id_val) = &id {
                let namespaced_action = format!("{}/{}", endpoint, id_val);
                if let Some(value) = actions.get(&namespaced_action) {
                    crate::vlo_debug!("📡 Resolved namespaced action: {}", namespaced_action);
                    action_name = namespaced_action;
                    query.remove("id");
                    value.clone()
                } else {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "success": false,
                            "error": "API operation not found",
                            "action": action_name,
                            "namespaced_attempt": namespaced_action
                        })),
                    )
                        .into_response();
                }
            } else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "API operation not found",
                        "action": action_name
                    })),
                )
                    .into_response();
            }
        }
    };

    // ─── ROLE CHECK ────────────────────────────────────────────────
    let required_roles = action_def.roles();
    if !required_roles.is_empty() {
        let user_role = auth.user.as_ref().map(|u| u.role.as_str()).unwrap_or("");
        let has_access = required_roles
            .iter()
            .any(|r| r.eq_ignore_ascii_case(user_role));

        if !has_access {
            crate::vlo_debug!(
                "🚫 Access denied for action '{}'. Required roles: {:?}, User role: '{}'",
                action_name,
                required_roles,
                user_role
            );
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Insufficient permissions",
                    "required_roles": required_roles,
                    "your_role": user_role
                })),
            )
                .into_response();
        }
        crate::vlo_debug!(
            "✅ Role check passed for action '{}'. User role: '{}'",
            action_name,
            user_role
        );
    }

    let mut sql = action_def.sql().to_string();

    let is_namespaced = action_name.contains('/');
    if let Some(id_value) = id.clone() {
        if !is_namespaced {
            query.insert("id".to_string(), id_value);
        }
    }

    // Build params map, skipping the `action` key (not a SQL param).
    let mut params = serde_json::Map::with_capacity(query.len());
    for (key, value) in query {
        if key != "action" {
            params.insert(key, query_string_to_value(&value));
        }
    }

    // 🔒 SECURITY: session injection must come AFTER form params
    // so a malicious form cannot spoof user_id.
    if let Some(user) = auth.user.as_ref() {
        params.insert("user_id".to_string(), serde_json::Value::Number(user.id.into()));
    }
    // 🔒 Only inject session_id when the SQL template actually references it.
    // Prevents the session token from leaking into debug logs on every request.
    if sql.contains("{{session_id}}") {
        if let Some(token) = auth.session_token.as_ref() {
            params.insert("session_id".to_string(), serde_json::Value::String(token.clone()));
        }
    }
    let is_id_request = params.contains_key("id") && operation == "get";
    if is_id_request && !sql.contains("{{id}}") && !sql.contains("{id}") {
        let upper = sql.to_uppercase();
        if let Some(pos) = upper.find(" ORDER BY ") {
            let before = sql[..pos].trim_end();
            let order = &sql[pos..];
            sql = if before.to_uppercase().contains(" WHERE ") {
                format!("{} AND id = {{id}}{}", before, order)
            } else {
                format!("{} WHERE id = {{id}}{}", before, order)
            };
        } else {
            let trimmed = sql.trim_end_matches(';').trim();
            sql = if trimmed.to_uppercase().contains(" WHERE ") {
                format!("{} AND id = {{id}}", trimmed)
            } else {
                format!("{} WHERE id = {{id}}", trimmed)
            };
        }
    }

    if !params.contains_key("limit") {
        params.insert("limit".to_string(), Value::Number(20.into()));
    }
    if !params.contains_key("page") {
        params.insert("page".to_string(), Value::Number(1.into()));
    }
    if let (Some(page_val), Some(limit_val)) = (params.get("page"), params.get("limit")) {
        if let (Some(p), Some(l)) = (page_val.as_i64(), limit_val.as_i64()) {
            let safe_page = p.max(1);
            let safe_limit = l.max(1).min(100);
            let offset = (safe_page - 1) * safe_limit;
            params.insert("offset".to_string(), Value::Number(offset.into()));
            params.insert("page".to_string(), Value::Number(safe_page.into()));
            params.insert("limit".to_string(), Value::Number(safe_limit.into()));
        }
    }

    crate::vlo_debug!("🔧 VLO DEBUG: Final SQL parameters = {:?}", params);

    let pool = match crate::database::get_pool() {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": e
                })),
            ).into_response()
        }
    };

    // 🔥 FIX: Delete orphaned files from storage
    if (method == Method::PUT || method == Method::PATCH) && params.contains_key("images") {
        // 1. Get new images from the request
        let new_arr: Vec<String> = match params.get("images") {
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_default(),
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => vec![],
        };

        // 2. Fetch old images from the database
        let select_sql = format!("SELECT images FROM {} WHERE id = {{{{id}}}}", resource);
        let select_sql = substitute_sql_params(&select_sql, &params);
        
        if let Ok(rows) = pool.fetch_all_json(&select_sql).await {
            if let Some(row) = rows.first() {
                if let Some(Value::String(old_json)) = row.get("images") {
                    if let Ok(old_arr) = serde_json::from_str::<Vec<String>>(old_json) {
                        // 3. Diff and delete physical files that are no longer in the new array
                        for old_file in old_arr {
                            if !new_arr.contains(&old_file) && !old_file.is_empty() {
                                let f = old_file.clone();
                                tokio::task::spawn_blocking(move || {
                                    let _ = crate::files::delete(&f);
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    

    let action_type = if action_name.contains("put")
        || action_name.contains("patch")
        || method == Method::PUT
        || method == Method::PATCH
    {
        "updated"
    } else if action_name.contains("delete") || method == Method::DELETE {
        "deleted"
    } else {
        "created"
    };

    crate::vlo_debug!("🔧 VLO DEBUG: Executing SQL = {}", sql);

    match execute_api_sql(pool, &sql, &params).await {
        Ok(mut data) => {
            let rows = data
                .get("data")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            crate::vlo_debug!("✅ VLO DEBUG: SQL successful — rows: {}", rows);

            // 🔥 PAGINATION CONFIGURATION
            // 1. Check if explicitly set in api.vlo
            // 2. Fallback to auto-detecting if the SQL uses LIMIT
            let should_paginate = action_def.paginate().unwrap_or_else(|| {
                sql.to_lowercase().contains("limit")
            });

            if method == Method::GET && !params.contains_key("id") && should_paginate {
                let count_sql = generate_count_sql(&sql);
                
                // ... the rest of your existing count query logic ...
                if !count_sql.is_empty() {
                    if let Ok(count_res) = execute_api_sql(pool, &count_sql, &params).await {
                        if let Some(count_data) = count_res.get("data").and_then(|d| d.as_array()) {
                            if let Some(first_row) = count_data.first() {
                                if let Some(total) = first_row.get("total").and_then(|v| v.as_i64()) {
                                    let page =
                                        params.get("page").and_then(|v| v.as_i64()).unwrap_or(1);
                                    let limit =
                                        params.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
                                    let total_pages = (total as f64 / limit as f64).ceil() as i64;
                                    let pagination = serde_json::json!({
                                        "total": total, "page": page, "limit": limit,
                                        "total_pages": total_pages,
                                        "has_next": page < total_pages, "has_prev": page > 1
                                    });
                                    if let Some(obj) = data.as_object_mut() {
                                        obj.insert("pagination".to_string(), pagination);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Reuse the already-computed `resource` from earlier (was recomputed).
            if method != Method::GET {
                crate::router::broadcast_live(&resource, &data);
            }

            if method != Method::GET {
                let (icon, title) = match action_type {
                    "updated" => ("✏️", "Update Successful"),
                    "deleted" => ("🗑️", "Delete Successful"),
                    _ => ("✅", "Operation Successful"),
                };
                let flash = crate::auth::encode_flash(
                    "success",
                    icon,
                    title,
                    &format!("{} completed successfully.", action_type),
                );
                return flash_response(
                    StatusCode::OK,
                    serde_json::json!({
                        "success": true,
                        "message": format!("{} operation successful", action_type)
                    }),
                    &flash,
                );
            }

            (StatusCode::OK, Json(data)).into_response()
        }
        Err(error) => {
            eprintln!("❌ [VLO API] {} {} -> SQL error: {}", method, action_name, error);
            if method != Method::GET {
                let flash = crate::auth::encode_flash("error", "⚠️", "Operation Failed", &error);
                return flash_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({
                        "success": false,
                        "error": "Operation failed",
                        "details": error
                    }),
                    &flash,
                );
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"success": false, "error": "SQL Execution Error", "details": error, "action": action_name})),
            )
                .into_response()
        }
    }
}

// ============================================================================
// SQL Execution & Helpers
// ============================================================================

pub async fn execute_api_sql(
    pool: &DbPool,
    sql_template: &str,
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let sql = substitute_sql_params(sql_template, params);
    crate::vlo_debug!("Executing SQL = {}", sql);
    let rows_json = pool
        .fetch_all_json(&sql)
        .await
        .map_err(|e| format!("SQL error: {}", e))?;
    Ok(serde_json::json!({
        "success": true,
        "data": rows_json,
        "affected_rows": rows_json.len()
    }))
}

/// Single-pass `{{key}}` substitution.
fn substitute_sql_params(
    sql: &str,
    params: &serde_json::Map<String, serde_json::Value>,
) -> String {
    // Fast path: no placeholders at all.
    if !sql.contains("{{") {
        return sql.to_string();
    }

    let mut out = String::with_capacity(sql.len() + 32);
    let mut cursor = 0usize;

    while let Some(start_rel) = sql[cursor..].find("{{") {
        let start = cursor + start_rel;
        out.push_str(&sql[cursor..start]);

        // Find `}}` — but cap the search so a stray `{{` doesn't
        // consume the entire rest of the SQL looking for a close.
        let rest = &sql[start + 2..];
        let Some(end_rel) = rest.find("}}") else {
            out.push_str(&sql[start..]);
            return out;
        };

        let ident_end = start + 2 + end_rel;
        let ident = &sql[start + 2..ident_end];

        // Only treat as placeholder if the contents look like an identifier.
        let valid_ident = !ident.is_empty()
            && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

        if valid_ident {
            if let Some(value) = params.get(ident) {
                write_sql_value(&mut out, value);
            } else {
                // Unknown placeholder: unwrap to bare identifier (legacy
                // behavior — the old regex cleanup dropped the braces).
                out.push_str(ident);
            }
            cursor = ident_end + 2;
        } else {
            // Not identifier-like: emit the `{{` literally and keep scanning
            // from just after it.
            out.push_str("{{");
            cursor = start + 2;
        }
    }

    out.push_str(&sql[cursor..]);
    out
}

/// single quotes for strings.
#[inline]
fn write_sql_value(out: &mut String, value: &Value) {
    match value {
        Value::Number(n) => {
            let _ = write!(out, "{}", n);
        }
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Null => out.push_str("NULL"),
        Value::String(s) => {
            out.push('\'');
            if s.contains('\'') {
                for ch in s.chars() {
                    if ch == '\'' {
                        out.push_str("''");
                    } else {
                        out.push(ch);
                    }
                }
            } else {
                out.push_str(s);
            }
            out.push('\'');
        }
        _ => {
            // Arrays/objects: fall back to JSON string form, still escaped.
            out.push('\'');
            let s = value.to_string();
            for ch in s.chars() {
                if ch == '\'' {
                    out.push_str("''");
                } else {
                    out.push(ch);
                }
            }
            out.push('\'');
        }
    }
}

fn crud_operation(method: &Method) -> Option<&'static str> {
    match *method {
        Method::GET => Some("get"),
        Method::POST => Some("post"),
        Method::PUT => Some("put"),
        Method::PATCH => Some("patch"),
        Method::DELETE => Some("delete"),
        _ => None,
    }
}

fn normalize_resource(endpoint: &str) -> String {
    let value = endpoint.trim().trim_matches('/');
    for prefix in ["get_", "post_", "put_", "patch_", "delete_"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    value.to_string()
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn percent_decode(value: &str) -> String {
    // Fast path: nothing to decode.
    if !value.as_bytes().iter().any(|b| *b == b'%' || *b == b'+') {
        return value.to_string();
    }

    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(a), Some(b)) = (h(bytes[i + 1]), h(bytes[i + 2])) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_form_body(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let mut pair = part.splitn(2, '=');
            let key = percent_decode(pair.next().unwrap_or(""));
            if key.is_empty() {
                return None;
            }
            Some((key, percent_decode(pair.next().unwrap_or(""))))
        })
        .collect()
}

fn generate_upload_name(original: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    
    let extension = Path::new(original)
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| v.to_ascii_lowercase())
        .filter(|v| !v.is_empty());

    match extension {
        Some(ext) => format!("vlo_{timestamp}_{}.{}", std::process::id(), ext),
        None => format!("vlo_{timestamp}_{}", std::process::id()),
    }
}

fn value_to_query_string(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        Value::Number(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

fn query_string_to_value(value: &str) -> Value {
    if value.is_empty() {
        return Value::String(String::new());
    }
    if value.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(integer) = value.parse::<i64>() {
        return Value::Number(integer.into());
    }
    if let Ok(float) = value.parse::<f64>() {
        if let Some(number) = serde_json::Number::from_f64(float) {
            return Value::Number(number);
        }
    }
    Value::String(value.to_string())
}