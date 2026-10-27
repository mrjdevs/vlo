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
use crate::features::{is_enabled, Feature};
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
        paginate: Option<bool>,
        #[serde(default)]
        flash: Option<bool>,   // 🔥 NEW
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

    pub fn flash(&self) -> Option<bool> {   // 🔥 NEW
        match self {
            ApiAction::Simple(_) => None,
            ApiAction::Protected { flash, .. } => *flash,
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

            // 🔥 NEW: Sanitize multi-line strings (Escapes raw newlines inside JSON strings)
            let sanitized = sanitize_json_multiline(&clean);

            match serde_json::from_str::<Value>(&sanitized) {
                Ok(json) => {
                    if let Some(object) = json.as_object() {
                        for (name, value) in object {
                            if let Ok(action) = serde_json::from_value::<ApiAction>(value.clone()) {
                                actions.insert(name.clone(), action);
                            } else {
                                eprintln!("❌ [VLO API] Failed to parse action definition '{}' in {:?}", name, file);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("❌ [VLO API] JSON Syntax Error in {:?}: {}", file, e);
                }
            }
        } // 👈 Added missing closing brace for `if let Some(block)`
    } // 👈 Added missing closing brace for `if let Ok(content)`
    
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
        method, endpoint, id
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
                    (StatusCode::OK, Json(serde_json::json!({"success": true, "actions": action_names}))).into_response()
                }
                Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Failed to load API definitions", "details": error}))).into_response(),
            };
        }
    };

    let resource = normalize_resource(&endpoint);
    if !valid_identifier(&resource) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"success": false, "error": "Invalid API resource"}))).into_response();
    }

    let operation = match crud_operation(&method) {
        Some(value) => value,
        None => return (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({"success": false, "error": "Unsupported HTTP method"}))).into_response(),
    };

    let actions = match load_api_actions() {
        Ok(value) => value,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Failed to load API definitions", "details": error}))).into_response(),
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
                    return (StatusCode::NOT_FOUND, Json(serde_json::json!({"success": false, "error": "API operation not found", "action": action_name, "namespaced_attempt": namespaced_action}))).into_response();
                }
            } else {
                return (StatusCode::NOT_FOUND, Json(serde_json::json!({"success": false, "error": "API operation not found", "action": action_name}))).into_response();
            }
        }
    };

    // ─── ROLE CHECK ────────────────────────────────────────────────
    // An action's declared method prefix must match the request method;
    // otherwise a GET could invoke a write action through a public route.
    if let Some(prefix) = ["get_", "post_", "put_", "patch_", "delete_"]
        .into_iter()
        .find(|prefix| action_name.starts_with(prefix))
    {
        let method_ok = match prefix {
            "get_" => matches!(method, Method::GET | Method::HEAD),
            "post_" => method == Method::POST,
            "put_" => method == Method::PUT,
            "patch_" => method == Method::PATCH,
            "delete_" => method == Method::DELETE,
            _ => false,
        };
        if !method_ok {
            return (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({"success": false, "error": "Action does not match HTTP method"}))).into_response();
        }
    }

    // Defense in depth: writes always require a session when auth is on,
    // even for actions that omit an explicit role list.
    let is_write_action = ["post_", "put_", "patch_", "delete_"]
        .iter()
        .any(|prefix| action_name.starts_with(prefix))
        || sql_has_write(action_def.sql());
    if is_write_action {
        if matches!(method, Method::GET | Method::HEAD) {
            return (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({"success": false, "error": "Write actions require a write HTTP method"}))).into_response();
        }
        if is_enabled(Feature::Auth) && auth.user.is_none() {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"success": false, "error": "Authentication required"}))).into_response();
        }
    }

    let required_roles = action_def.roles();
    if !required_roles.is_empty() {
        let user_role = auth.user.as_ref().map(|u| u.role.as_str()).unwrap_or("");
        let has_access = required_roles.iter().any(|r| r.eq_ignore_ascii_case(user_role));
        if !has_access {
            crate::vlo_debug!("🚫 Access denied for action '{}'. Required roles: {:?}, User role: '{}'", action_name, required_roles, user_role);
            return (StatusCode::FORBIDDEN, Json(serde_json::json!({"success": false, "error": "Insufficient permissions", "required_roles": required_roles, "your_role": user_role}))).into_response();
        }
        crate::vlo_debug!("✅ Role check passed for action '{}'. User role: '{}'", action_name, user_role);
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
    if let Some(user) = auth.user.as_ref() {
        params.insert("user_id".to_string(), serde_json::Value::Number(user.id.into()));
        params.insert("user_role".to_string(), serde_json::Value::String(user.role.clone()));
    }
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
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": e}))).into_response(),
    };

    // 🔥 Orphaned file cleanup (unchanged)
    if (method == Method::PUT || method == Method::PATCH) && params.contains_key("images") {
        let new_arr: Vec<String> = match params.get("images") {
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_default(),
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => vec![],
        };
        let select_sql = format!("SELECT images FROM {} WHERE id = {{{{id}}}}", resource);
        let select_sql = substitute_sql_params(&select_sql, &params);
        if let Ok(rows) = pool.fetch_all_json(&select_sql).await {
            if let Some(row) = rows.first() {
                if let Some(Value::String(old_json)) = row.get("images") {
                    if let Ok(old_arr) = serde_json::from_str::<Vec<String>>(old_json) {
                        for old_file in old_arr {
                            if !new_arr.contains(&old_file) && !old_file.is_empty() {
                                let f = old_file.clone();
                                tokio::task::spawn_blocking(move || { let _ = crate::files::delete(&f); });
                            }
                        }
                    }
                }
            }
        }
    }

    // 🔥 EXTRACT `{{emit:event.name}}` markers from SQL
    let (sql, custom_events) = extract_emit_markers(&sql);

    let action_type = if action_name.contains("put") || action_name.contains("patch") || method == Method::PUT || method == Method::PATCH {
        "updated"
    } else if action_name.contains("delete") || method == Method::DELETE {
        "deleted"
    } else {
        "created"
    };

    crate::vlo_debug!("🔧 VLO DEBUG: Executing SQL = {}", sql);

    match execute_api_sql(pool, &sql, &params).await {
        Ok(mut data) => {
            let rows = data.get("data").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            crate::vlo_debug!("✅ VLO DEBUG: SQL successful — rows: {}", rows);

            // Pagination (unchanged)
            let should_paginate = action_def.paginate().unwrap_or_else(|| sql.to_lowercase().contains("limit"));
            if method == Method::GET && !params.contains_key("id") && should_paginate {
                let count_sql = generate_count_sql(&sql);
                if !count_sql.is_empty() {
                    if let Ok(count_res) = execute_api_sql(pool, &count_sql, &params).await {
                        if let Some(count_data) = count_res.get("data").and_then(|d| d.as_array()) {
                            if let Some(first_row) = count_data.first() {
                                if let Some(total) = first_row.get("total").and_then(|v| v.as_i64()) {
                                    let page = params.get("page").and_then(|v| v.as_i64()).unwrap_or(1);
                                    let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
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

            // ─── 🔥 AUTO-EMIT: fire events from naming convention + explicit markers ───
            if method != Method::GET && is_enabled(Feature::Jobs) {
                let context = data.get("data")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let user_id = auth.user.as_ref().map(|u| u.id);

                // Naming convention: post_orders → order.created
                if let Some(event) = derive_event_type(&action_name, &method) {
                    if let Err(e) = crate::jobs::auto_emit_events(pool, &event, &context, user_id).await {
                        crate::vlo_debug!("⚠️ [AUTO-EMIT] Failed for '{}': {}", event, e);
                    }
                }

                // Explicit markers: {{emit:order.shipped}}
                for event in &custom_events {
                    if let Err(e) = crate::jobs::auto_emit_events(pool, event, &context, user_id).await {
                        crate::vlo_debug!("⚠️ [AUTO-EMIT] Failed for marker '{}': {}", event, e);
                    }
                }
            }

            // Broadcast live update for mutations
            if method != Method::GET {
                crate::router::broadcast_live(&resource, &data);
            }

            // ─── 🔥 FLASH: respect per-action opt-out ───
            if method != Method::GET {
                let should_flash = action_def.flash().unwrap_or(true); // default true (backward compat)
            if should_flash {
                let (icon, title) = match action_type {
                    "updated" => ("✏️", "Update Successful"),
                    "deleted" => ("🗑️", "Delete Successful"),
                    _         => ("✅", "Operation Successful"),
                };
                let flash = crate::auth::encode_flash(
                    "success", icon, title,
                    &format!("{} completed successfully.", action_type),
                );

                // 🔥 Preserve redirect from SQL result (e.g., post_orders → thank you)
                let mut body = serde_json::json!({
                    "success": true,
                    "message": format!("{} operation successful", action_type)
                });
            if let Some(rows) = data.get("data").and_then(|v| v.as_array()) {
                if let Some(first) = rows.first() {
                    if let Some(redirect) = first.get("redirect").and_then(|v| v.as_str()) {
                        // Some actions return json_object → double-wrapped string
                        // Unwrap: '{"redirect":"/url"}' → '/url'
                        let url = if redirect.starts_with('{') {
                            serde_json::from_str::<serde_json::Value>(redirect)
                                .ok()
                                .and_then(|v| v.get("redirect").and_then(|r| r.as_str()).map(String::from))
                                .unwrap_or_else(|| redirect.to_string())
                        } else {
                            redirect.to_string()
                        };
                        body["redirect"] = serde_json::json!(url);
                    }
                }
            }

                return flash_response(StatusCode::OK, body, &flash);
            }
            return (StatusCode::OK, Json(data)).into_response();
            }

            (StatusCode::OK, Json(data)).into_response()
        }
        Err(error) => {
            eprintln!("❌ [VLO API] {} {} -> SQL error: {}", method, action_name, error);
            if method != Method::GET {
                // Errors always flash unless explicitly opted out
                let should_flash = action_def.flash().unwrap_or(true);
                if should_flash {
                    let flash = crate::auth::encode_flash("error", "⚠️", "Operation Failed", &error);
                    return flash_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        serde_json::json!({"success": false, "error": "Operation failed", "details": error}),
                        &flash,
                    );
                }
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Operation failed", "details": error}))).into_response();
            }
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "SQL Execution Error", "details": error, "action": action_name}))).into_response()
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
    // 1. Substitute parameters
    let raw_sql = substitute_sql_params(sql_template, params);
    
    // 2. Translate dialect (SQLite -> Postgres/MySQL)
    let translated_sql = pool.translate_sql(&raw_sql);
    
    crate::vlo_debug!("Executing SQL = {}", translated_sql);

    // 3. Split into statements and strip manual BEGIN/COMMIT
    let stmts: Vec<String> = split_sql_statements(&translated_sql)
        .into_iter()
        .filter(|s| {
            let up = s.to_uppercase();
            !up.starts_with("BEGIN")
                && !up.starts_with("COMMIT")
                && !up.starts_with("ROLLBACK")
                && !up.starts_with("SAVEPOINT")
                && !up.starts_with("RELEASE")
        })
        .collect();

    if stmts.is_empty() {
        return Ok(serde_json::json!({
            "success": true, "data": [], "affected_rows": 0
        }));
    }

    // 4. Fast path: single statement (no transaction overhead)
    if stmts.len() == 1 {
        let rows = pool.fetch_all_json(&stmts[0]).await
            .map_err(|e| format!("SQL error: {}", e))?;
        return Ok(serde_json::json!({
            "success": true, "data": rows, "affected_rows": rows.len()
        }));
    }

    // 5. Multi-statement: run atomically in a transaction
    let last_rows = pool.execute_multi_statement(&stmts).await?;
    Ok(serde_json::json!({
        "success": true, "data": last_rows, "affected_rows": last_rows.len()
    }))
}

/// Splits a SQL script on `;` outside quoted strings, quoted identifiers and
/// comments. Substituted values may contain semicolons (addresses, notes), so
/// a plain `split(';')` corrupts the statement into an unterminated literal.
fn split_sql_statements(sql: &str) -> Vec<String> {
    enum State {
        Normal,
        SingleQuote,
        DoubleQuote,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    let mut statements = Vec::new();
    let mut current = String::new();
    let mut state = State::Normal;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            State::Normal => match c {
                '\'' => { state = State::SingleQuote; current.push(c); }
                '"' => { state = State::DoubleQuote; current.push(c); }
                '`' => { state = State::Backtick; current.push(c); }
                '[' => { state = State::Bracket; current.push(c); }
                '-' if chars.peek() == Some(&'-') => {
                    state = State::LineComment;
                    current.push(c);
                    current.push(chars.next().unwrap());
                }
                '/' if chars.peek() == Some(&'*') => {
                    state = State::BlockComment;
                    current.push(c);
                    current.push(chars.next().unwrap());
                }
                ';' => {
                    let trimmed = current.trim();
                    if !trimmed.is_empty() { statements.push(trimmed.to_string()); }
                    current.clear();
                }
                _ => current.push(c),
            },
            State::SingleQuote => {
                current.push(c);
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        current.push(chars.next().unwrap());
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::DoubleQuote => {
                current.push(c);
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        current.push(chars.next().unwrap());
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::Backtick => {
                current.push(c);
                if c == '`' {
                    if chars.peek() == Some(&'`') {
                        current.push(chars.next().unwrap());
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::Bracket => {
                current.push(c);
                if c == ']' { state = State::Normal; }
            }
            State::LineComment => {
                current.push(c);
                if c == '\n' { state = State::Normal; }
            }
            State::BlockComment => {
                current.push(c);
                if c == '*' && chars.peek() == Some(&'/') {
                    current.push(chars.next().unwrap());
                    state = State::Normal;
                }
            }
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() { statements.push(trimmed.to_string()); }
    statements
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

fn sql_has_write(sql: &str) -> bool {
    split_sql_statements(sql).iter().any(|statement| {
        let first = statement
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '(')
            .find(|token| !token.is_empty())
            .unwrap_or("")
            .to_ascii_uppercase();
        matches!(
            first.as_str(),
            "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "CREATE" | "DROP" | "ALTER"
                | "TRUNCATE" | "PRAGMA" | "ATTACH" | "DETACH"
        )
    })
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

// ---------------------------------------------------------------------------
// 🔥 AUTO-EMIT HELPERS
// ---------------------------------------------------------------------------

/// Derive event name from action: `post_orders` → `order.created`
fn derive_event_type(action: &str, method: &Method) -> Option<String> {
    if *method == Method::GET { return None; }

    let (verb, resource) = if let Some(r) = action.strip_prefix("post_") {
        ("created", r)
    } else if let Some(r) = action.strip_prefix("put_").or_else(|| action.strip_prefix("patch_")) {
        ("updated", r)
    } else if let Some(r) = action.strip_prefix("delete_") {
        ("deleted", r)
    } else {
        return None;
    };

    if resource.is_empty() { return None; }

    // 🔒 Skip framework-owned resources — they must not auto-emit events.
    // Prevents infinite loops like: adding an email_rule → fires
    // email_rule.created → a rule for that event sends mail → adds more rules.
    // Also covers vlo_jobs, vlo_sessions, vlo_auth_tokens, and any future
    // framework table that's prefixed with `vlo_`.
    if resource.starts_with("vlo_")
        || matches!(resource,
            "email_rules"
            | "email_rules_stats"
            | "jobs"
            | "sessions"
            | "auth_tokens"
        )
    {
        return None;
    }

    // Singularize
    let singular = if resource.ends_with("ies") && resource.len() > 3 {
        format!("{}y", &resource[..resource.len() - 3])
    } else if resource.ends_with('s') && !resource.ends_with("ss") {
        resource[..resource.len() - 1].to_string()
    } else {
        resource.to_string()
    };

    // Domain-specific rename: user registration
    if resource == "users" && verb == "created" {
        return Some("user.registered".to_string());
    }

    Some(format!("{}.{}", singular, verb))
}

/// Extract `{{emit:event.name}}` markers from SQL.
/// Returns (cleaned_sql, events).
fn extract_emit_markers(sql: &str) -> (String, Vec<String>) {
    if !sql.contains("{{emit:") {
        return (sql.to_string(), Vec::new());
    }
    let mut events = Vec::new();
    let mut cleaned = String::with_capacity(sql.len());
    let mut rest = sql;
    loop {
        let Some(start) = rest.find("{{emit:") else {
            cleaned.push_str(rest);
            break;
        };
        cleaned.push_str(&rest[..start]);
        let after = &rest[start + 7..];
        let Some(end) = after.find("}}") else {
            cleaned.push_str(&rest[start..]);
            break;
        };
        events.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    (cleaned, events)
}

/// Pre-processes the JSON block to allow raw multi-line strings.
/// Escapes newlines inside string literals so serde_json doesn't crash.
pub(crate) fn sanitize_json_multiline(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escape_next = false;

    for c in input.chars() {
        if escape_next {
            out.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' if in_string => {
                out.push(c);
                escape_next = true;
            }
            '"' => {
                out.push(c);
                in_string = !in_string;
            }
            '\n' | '\r' if in_string => {
                // Convert raw newlines to escaped \n
                out.push_str("\\n");
            }
            _ => {
                out.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::split_sql_statements;

    #[test]
    fn splits_outside_quotes_comments_and_identifiers() {
        let sql = "INSERT INTO t (a) VALUES ('uh; g'); UPDATE t SET a = 'x;y' WHERE b = \"c;d\"; -- tail; comment\nSELECT 1; /* block; */ SELECT 2";
        let parts = split_sql_statements(sql);
        assert_eq!(parts.len(), 4);
        assert!(parts[0].contains("'uh; g'"));
        assert!(parts[1].contains("'x;y'"));
        assert!(parts[1].contains("\"c;d\""));
        assert_eq!(parts[3], "/* block; */ SELECT 2");
    }

    #[test]
    fn keeps_escaped_quote_semicolons() {
        let parts = split_sql_statements("SELECT 'it''s; fine'; SELECT 2");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "SELECT 'it''s; fine'");
    }
}