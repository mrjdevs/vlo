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
}

// ============================================================================
// API Action Cache
// ============================================================================

static API_ACTIONS_CACHE: LazyLock<Mutex<Option<CachedActions>>> = LazyLock::new(|| Mutex::new(None));

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
            let clean = block.trim_start_matches('\u{feff}').replace('\u{a0}', " ").replace('\r', "");
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
            if modified > latest { latest = modified; }
        }
    }
    if let Ok(entries) = fs::read_dir(api_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let ns_file = path.join("api.vlo");
                if let Ok(meta) = fs::metadata(&ns_file) {
                    if let Ok(modified) = meta.modified() {
                        if modified > latest { latest = modified; }
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
    let modified = if is_dev { get_api_modified_time(&api_dir) } else { SystemTime::UNIX_EPOCH };

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
            if !path.is_dir() { continue; }
            let namespace = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let ns_file = path.join("api.vlo");
            if !ns_file.exists() { continue; }
            let ns_actions = load_actions_from_file(&ns_file);
            crate::vlo_debug!("📡 Loaded {} actions from api/{}/api.vlo", ns_actions.len(), namespace);
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
// HTTP Handlers (Updated to extract AuthUser)
// ============================================================================

pub async fn api_handler_root(
    Extension(auth): Extension<crate::auth::AuthUser>,
    req: Request,
) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => api_route_handler(None, None, method, query, &auth).await.into_response(),
        Err(response) => response,
    }
}

pub async fn api_handler_path(
    Extension(auth): Extension<crate::auth::AuthUser>,
    AxumPath(resource): AxumPath<String>,
    req: Request,
) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => api_route_handler(Some(resource), None, method, query, &auth).await.into_response(),
        Err(response) => response,
    }
}

pub async fn api_handler_id(
    Extension(auth): Extension<crate::auth::AuthUser>,
    AxumPath((resource, id)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => api_route_handler(Some(resource), Some(id), method, query, &auth).await.into_response(),
        Err(response) => response,
    }
}

async fn prepare_api_request(req: Request) -> Result<(Method, HashMap<String, String>), Response> {
    let method = req.method().clone();
    let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let mut query = req.uri().query().map(parse_form_body).unwrap_or_default();

    if content_type.starts_with("multipart/form-data") {
        let mut multipart = Multipart::from_request(req, &()).await.map_err(|e| {
            (StatusCode::BAD_REQUEST, Json(serde_json::json!({"success": false, "error": "Invalid multipart request", "details": e.to_string()}))).into_response()
        })?;
        parse_multipart_body(&mut multipart, &mut query).await?;
    } else {
        let body = Bytes::from_request(req, &()).await.map_err(|e| {
            (StatusCode::BAD_REQUEST, Json(serde_json::json!({"success": false, "error": "Invalid request body", "details": e.to_string()}))).into_response()
        })?;
        if !body.is_empty() {
            if content_type.contains("application/json") {
                if let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(&body) {
                    for (key, value) in object { query.insert(key, value_to_query_string(&value)); }
                }
            } else if content_type.contains("application/x-www-form-urlencoded") {
                for (key, value) in parse_form_body(&String::from_utf8_lossy(&body)) { query.insert(key, value); }
            }
        }
    }
    Ok((method, query))
}

async fn parse_multipart_body(multipart: &mut Multipart, query: &mut HashMap<String, String>) -> Result<(), Response> {
    let max_size = crate::state::max_upload_bytes();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"success": false, "error": "Invalid multipart", "details": e.to_string()}))).into_response()),
        };
        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(|n| n.to_string());
        let bytes = field.bytes().await.map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"success": false, "error": "Failed to read file", "details": e.to_string()}))).into_response())?;
        if bytes.len() as u64 > max_size {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, Json(serde_json::json!({"success": false, "error": "File too large"}))).into_response());
        }
        if let Some(original_name) = file_name {
            let upload_name = generate_upload_name(&original_name);
            let upload_name_clone = upload_name.clone();
            let bytes_clone = bytes.clone();
            let save_result = tokio::task::spawn_blocking(move || crate::files::save(&upload_name_clone, &bytes_clone)).await;
            match save_result {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Failed to save file", "details": e.to_string()}))).into_response()),
                Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Failed to save file", "details": e.to_string()}))).into_response()),
            }
            query.insert(name, format!("/uploads/{}", upload_name));
        } else {
            query.insert(name, String::from_utf8_lossy(&bytes).to_string());
        }
    }
    Ok(())
}

// ============================================================================
// Auto-Pagination Helper
// ============================================================================

static RE_LIMIT: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?i)\s+LIMIT\s+\{\{.*?\}\}").unwrap());
static RE_OFFSET: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?i)\s+OFFSET\s+\{\{.*?\}\}").unwrap());

pub fn generate_count_sql(sql_template: &str) -> String {
    let without_limit = RE_LIMIT.replace_all(sql_template, "");
    let stripped = RE_OFFSET.replace_all(&without_limit, "");
    let stripped = stripped.trim().trim_end_matches(';');
    if stripped.is_empty() { return String::new(); }
    format!("SELECT COUNT(*) as total FROM ({}) AS _vlo_count_subquery", stripped)
}

// ============================================================================
// Flash Message Helper
// ============================================================================

fn flash_response(status: StatusCode, body: serde_json::Value, flash_encoded: &str) -> Response {
    let cookie = crate::auth::flash_cookie_header(flash_encoded);
    let mut response = (status, Json(body)).into_response();
    if let Ok(val) = axum::http::HeaderValue::from_str(&cookie) {
        response.headers_mut().append(axum::http::header::SET_COOKIE, val);
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
    crate::vlo_debug!("🔧 VLO DEBUG: API request started | Method: {} | Endpoint: {:?} | ID: {:?}", method, endpoint, id);

    if endpoint.is_none() {
        if let Some(action) = query.get("action").cloned() { endpoint = Some(action); }
    }

    let endpoint = match endpoint {
        Some(value) => value.trim().trim_matches('/').to_string(),
        None => {
            return match load_api_actions() {
                Ok(actions) => {
                    let action_names: Vec<&String> = actions.keys().collect();
                    (StatusCode::OK, Json(serde_json::json!({"success": true, "actions": action_names}))).into_response()
                },
                Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Failed to load API definitions", "details": error}))).into_response(),
            };
        }
    };

    let resource = normalize_resource(&endpoint);
    if !valid_identifier(&resource) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "success": false,
            "error": "Invalid API resource"
        }))).into_response();
    }

    let operation = match crud_operation(&method) {
        Some(value) => value,
        None => return (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({"success": false, "error": "Unsupported HTTP method"}))).into_response(),
    };

    let actions = match load_api_actions() {
        Ok(value) => value,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Failed to load API definitions", "details": error}))).into_response(),
    };

    let explicit_action = ["get_", "post_", "put_", "patch_", "delete_"].iter().any(|prefix| endpoint.starts_with(prefix));
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
                    return (StatusCode::NOT_FOUND, Json(serde_json::json!({
                        "success": false,
                        "error": "API operation not found",
                        "action": action_name,
                        "namespaced_attempt": namespaced_action
                    }))).into_response();
                }
            } else {
                return (StatusCode::NOT_FOUND, Json(serde_json::json!({
                    "success": false,
                    "error": "API operation not found",
                    "action": action_name
                }))).into_response();
            }
        }
    };

    // ─── ROLE CHECK 🔥 ─────────────────────────────────────────────
    let required_roles = action_def.roles();
    if !required_roles.is_empty() {
        let user_role = auth.user.as_ref().map(|u| u.role.as_str()).unwrap_or("");
        let has_access = required_roles.iter().any(|r| r.eq_ignore_ascii_case(user_role));

        if !has_access {
            crate::vlo_debug!("🚫 Access denied for action '{}'. Required roles: {:?}, User role: '{}'", action_name, required_roles, user_role);
            return (StatusCode::FORBIDDEN, Json(serde_json::json!({
                "success": false,
                "error": "Insufficient permissions",
                "required_roles": required_roles,
                "your_role": user_role
            }))).into_response();
        }
        crate::vlo_debug!("✅ Role check passed for action '{}'. User role: '{}'", action_name, user_role);
    }

    let mut sql = action_def.sql().to_string();
    // ────────────────────────────────────────────────────────────────

    let is_namespaced = action_name.contains('/');
    if let Some(id_value) = id.clone() {
        if !is_namespaced {
            query.insert("id".to_string(), id_value);
        }
    }

    let mut params = serde_json::Map::new();
    for (key, value) in query {
        if key != "action" { params.insert(key, query_string_to_value(&value)); }
    }

    let is_id_request = params.contains_key("id") && operation == "get";
    if is_id_request && !sql.contains("{{id}}") && !sql.contains("{id}") {
        let upper = sql.to_uppercase();
        if let Some(pos) = upper.find(" ORDER BY ") {
            let before = sql[..pos].trim_end();
            let order = &sql[pos..];
            sql = if before.to_uppercase().contains(" WHERE ") { format!("{} AND id = {{id}}{}", before, order) }
                  else { format!("{} WHERE id = {{id}}{}", before, order) };
        } else {
            let trimmed = sql.trim_end_matches(';').trim();
            sql = if trimmed.to_uppercase().contains(" WHERE ") { format!("{} AND id = {{id}}", trimmed) }
                  else { format!("{} WHERE id = {{id}}", trimmed) };
        }
    }

    if !params.contains_key("limit") { params.insert("limit".to_string(), Value::Number(20.into())); }
    if !params.contains_key("page") { params.insert("page".to_string(), Value::Number(1.into())); }
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
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "success": false, 
            "error": e
        }))).into_response()
    };

    let action_type = if action_name.contains("put") || action_name.contains("patch") || method == Method::PUT || method == Method::PATCH { "updated" }
                      else if action_name.contains("delete") || method == Method::DELETE { "deleted" }
                      else { "created" };

    crate::vlo_debug!("🔧 VLO DEBUG: Executing SQL = {}", sql);

    match execute_api_sql(pool, &sql, &params).await {
        Ok(mut data) => {
            let rows = data.get("data").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            crate::vlo_debug!("✅ VLO DEBUG: SQL successful — rows: {}", rows);

            if method == Method::GET && !params.contains_key("id") {
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

            if method != Method::GET {
                let resource = normalize_resource(&endpoint);
                crate::router::broadcast_live(&resource, &data);
            }

            if method != Method::GET {
                let (icon, title) = match action_type {
                    "updated" => ("✏️", "Update Successful"),
                    "deleted" => ("🗑️", "Delete Successful"),
                    _ => ("✅", "Operation Successful"),
                };
                let flash = crate::auth::encode_flash("success", icon, title, &format!("{} completed successfully.", action_type));
                return flash_response(StatusCode::OK, serde_json::json!({
                    "success": true,
                    "message": format!("{} operation successful", action_type)
                }), &flash);
            }

            (StatusCode::OK, Json(data)).into_response()
        }
        Err(error) => {
            eprintln!("❌ [VLO API] {} {} -> SQL error: {}", method, action_name, error);
            if method != Method::GET {
                let flash = crate::auth::encode_flash("error", "⚠️", "Operation Failed", &error);
                return flash_response(StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({
                    "success": false,
                    "error": "Operation failed",
                    "details": error
                }), &flash);
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
    let mut sql = sql_template.to_string();
    for (key, value) in params {
        let placeholder = format!("{{{{{}}}}}", key);
        let replacement = match value {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null => "NULL".to_string(),
            serde_json::Value::String(s) => format!("'{}'", s.replace("'", "''")),
            _ => format!("'{}'", value.to_string().replace("'", "''")),
        };
        sql = sql.replace(&placeholder, &replacement);
    }
    let re = regex::Regex::new(r"\{\{([a-zA-Z0-9_]+)\}\}").unwrap();
    sql = re.replace_all(&sql, "$1").into_owned();
    crate::vlo_debug!("Executing SQL = {}", sql);
    let rows_json = pool.fetch_all_json(&sql).await.map_err(|e| format!("SQL error: {}", e))?;
    Ok(serde_json::json!({
        "success": true,
        "data": rows_json,
        "affected_rows": rows_json.len()
    }))
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
        if let Some(rest) = value.strip_prefix(prefix) { return rest.to_string(); }
    }
    value.to_string()
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' { out.push(b' '); i += 1; continue; }
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = |c: u8| -> Option<u8> {
                match c { b'0'..=b'9' => Some(c - b'0'), b'a'..=b'f' => Some(c - b'a' + 10), b'A'..=b'F' => Some(c - b'A' + 10), _ => None }
            };
            if let (Some(a), Some(b)) = (h(bytes[i + 1]), h(bytes[i + 2])) { out.push(a * 16 + b); i += 3; continue; }
        }
        out.push(bytes[i]); i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_form_body(body: &str) -> HashMap<String, String> {
    body.split('&').filter(|part| !part.is_empty()).filter_map(|part| {
        let mut pair = part.splitn(2, '=');
        let key = percent_decode(pair.next().unwrap_or(""));
        if key.is_empty() { return None; }
        Some((key, percent_decode(pair.next().unwrap_or(""))))
    }).collect()
}

fn generate_upload_name(original: &str) -> String {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default();
    let extension = Path::new(original).extension().and_then(|v| v.to_str()).map(|v| v.to_ascii_lowercase()).filter(|v| !v.is_empty());
    match extension {
        Some(ext) => format!("vlo_{}{}.{}", timestamp, std::process::id(), ext),
        None => format!("vlo{}_{}", timestamp, std::process::id()),
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
    if value.is_empty() { return Value::String(value.to_string()); }
    if value.eq_ignore_ascii_case("true") { return Value::Bool(true); }
    if value.eq_ignore_ascii_case("false") { return Value::Bool(false); }
    if let Ok(integer) = value.parse::<i64>() { return Value::Number(integer.into()); }
    if let Ok(float) = value.parse::<f64>() {
        if let Some(number) = serde_json::Number::from_f64(float) { return Value::Number(number); }
    }
    Value::String(value.to_string())
}