use crate::{
    database::{DbPool, DB_POOL},
    state::get_project_root,
};
use axum::{
    body::Bytes,
    extract::{FromRequest, Multipart, Path as AxumPath, Request},
    http::{Method, StatusCode},
    response::{IntoResponse, Json, Redirect, Response},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

// ---------------------------------------------------------------------------
// API Action Cache
// ---------------------------------------------------------------------------
struct CachedActions {
    actions: HashMap<String, String>,
    modified: SystemTime,
}

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

pub fn load_api_actions() -> Result<HashMap<String, String>, String> {
    let file = get_project_root().join("pages/api/api.vlo");
    let metadata = fs::metadata(&file).map_err(|_| format!("API file not found: {}", file.display()))?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);

    if let Ok(cache) = API_ACTIONS_CACHE.lock() {
        if let Some(cached) = cache.as_ref() {
            if cached.modified == modified {
                crate::vlo_debug!("⚡ API actions served from memory cache");
                return Ok(cached.actions.clone());
            }
        }
    }

    let content = fs::read_to_string(&file).map_err(|e| format!("Could not read {}: {}", file.display(), e))?;
    let block = extract_server_block(&content).ok_or_else(|| format!("No <script server> block in {}", file.display()))?;
    let clean = block.trim_start_matches('\u{feff}').replace('\u{a0}', " ").replace('\r', "");
    
    let json: Value = serde_json::from_str(&clean).map_err(|e| format!("Invalid JSON in {}: {}", file.display(), e))?;
    let object = json.as_object().ok_or_else(|| "API definitions must be a JSON object".to_string())?;

    let mut actions = HashMap::new();
    for (name, value) in object {
        if let Some(sql) = value.as_str() {
            actions.insert(name.clone(), sql.to_string());
        }
    }

    if let Ok(mut cache) = API_ACTIONS_CACHE.lock() {
        *cache = Some(CachedActions { actions: actions.clone(), modified });
    }
    Ok(actions)
}

// ---------------------------------------------------------------------------
// HTTP Handlers
// ---------------------------------------------------------------------------
pub async fn api_handler_root(req: Request) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => api_route_handler(None, None, method, query).await.into_response(),
        Err(response) => response,
    }
}

pub async fn api_handler_path(AxumPath(resource): AxumPath<String>, req: Request) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => api_route_handler(Some(resource), None, method, query).await.into_response(),
        Err(response) => response,
    }
}

pub async fn api_handler_id(AxumPath((resource, id)): AxumPath<(String, String)>, req: Request) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => api_route_handler(Some(resource), Some(id), method, query).await.into_response(),
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

// ---------------------------------------------------------------------------
// Auto-Pagination Helper
// ---------------------------------------------------------------------------
pub fn generate_count_sql(sql_template: &str) -> String {
    let re_limit = regex::Regex::new(r"(?i)\s+LIMIT\s+\{\{.*?\}\}").unwrap();
    let re_offset = regex::Regex::new(r"(?i)\s+OFFSET\s+\{\{.*?\}\}").unwrap();
    
    let without_limit = re_limit.replace_all(sql_template, "");
    let stripped = re_offset.replace_all(&without_limit, "");
    
    let stripped = stripped.trim().trim_end_matches(';');
    if stripped.is_empty() { return String::new(); }
    format!("SELECT COUNT(*) as total FROM ({}) AS _vlo_count_subquery", stripped)
}

// ---------------------------------------------------------------------------
// Main Route Handler
// ---------------------------------------------------------------------------
pub async fn api_route_handler(
    mut endpoint: Option<String>,
    id: Option<String>,
    method: Method,
    mut query: HashMap<String, String>,
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
                Ok(actions) => (StatusCode::OK, Json(serde_json::json!({"success": true, "actions": actions}))).into_response(),
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

    let explicit_action = ["get_", "post_", "put_", "patch_", "delete_"].iter().any(|prefix| endpoint.starts_with(prefix));
    let action_name = if explicit_action { endpoint.clone() } else { format!("{}_{}", operation, resource) };

    let actions = match load_api_actions() {
        Ok(value) => value,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Failed to load API definitions", "details": error}))).into_response(),
    };

    let mut sql = match actions.get(&action_name) {
        Some(value) => value.clone(),
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"success": false, "error": "API operation not found", "action": action_name}))).into_response(),
    };

    if let Some(id_value) = id.clone() { query.insert("id".to_string(), id_value); }

    if id.is_some() && operation == "get" && !sql.contains("{{id}}") && !sql.contains("{id}") {
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

    let mut params = serde_json::Map::new();
    for (key, value) in query {
        if key != "action" { params.insert(key, query_string_to_value(&value)); }
    }

    // ─── DEFAULT PAGINATION PARAMS ───────────────────────────
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
    // ─────────────────────────────────────────────────────────

    crate::vlo_debug!("🔧 VLO DEBUG: Final SQL parameters = {:?}", params);

    let pool = match DB_POOL.get() {
        Some(p) => p,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "Database not configured"}))).into_response(),
    };

    let action_type = if action_name.starts_with("put") || action_name.starts_with("patch") || method == Method::PUT || method == Method::PATCH { "updated" }
                      else if action_name.starts_with("delete") || method == Method::DELETE { "deleted" }
                      else { "created" };

    crate::vlo_debug!("🔧 VLO DEBUG: Executing SQL = {}", sql);

    match execute_api_sql(pool, &sql, &params).await {
        Ok(mut data) => {
            let rows = data
                .get("data")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);

            let first = data
                .get("data")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".to_string());

            crate::vlo_debug!(
                "✅ VLO DEBUG: SQL successful — rows: {}, first Object: {}",
                rows,
                first
            );

            // ─── AUTO-PAGINATION FOR GET LIST ENDPOINTS ──────────────────────
            if method == Method::GET && id.is_none() {
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
            // ─────────────────────────────────────────────────────────────────

            if method != Method::GET {
                return Redirect::to(&format!("/{}?status=success&action={}", resource, action_type)).into_response();
            }
            (StatusCode::OK, Json(data)).into_response()
        }
        Err(error) => {
            eprintln!("❌ [VLO API] {} {} -> SQL error: {}", method, action_name, error);
            if method != Method::GET {
                return Redirect::to(&format!("/{}?status=error&action={}", resource, action_type)).into_response();
            }
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"success": false, "error": "SQL Execution Error", "details": error, "action": action_name}))).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// SQL Execution & Helpers
// ---------------------------------------------------------------------------
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
    
    crate::vlo_debug!("Executing SQL = {}", sql);

    // One line replaces 30 lines of database-specific match blocks!
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
        Some(ext) => format!("vlo_{}_{}.{}", timestamp, std::process::id(), ext),
        None => format!("vlo_{}_{}", timestamp, std::process::id()),
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