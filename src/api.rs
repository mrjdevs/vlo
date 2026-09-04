use crate::{
    database::{DbPool, DB_POOL},
    state::get_project_root,
};
use axum::{
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Json, Redirect},
};
use serde_json::Value;
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::{collections::HashMap, fs};

pub fn extract_server_block(content: &str) -> Option<String> {
    crate::vlo_debug!("🔧 VLO DEBUG: Extracting <script server> block");

    let start = content.find("<script server>")?;
    let rest = &content[start + 15..];
    let end = rest.find("</script>")?;

    let block = rest[..end].trim().to_string();

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Server block extracted ({} bytes)",
        block.len()
    );

    Some(block)
}

pub fn strip_server_block(content: &str) -> String {
    crate::vlo_debug!("🔧 VLO DEBUG: Stripping <script server> block");

    if let Some(start) = content.find("<script server>") {
        if let Some(end) = content[start..].find("</script>") {
            let end = start + end + "</script>".len();

            crate::vlo_debug!(
                "🔧 VLO DEBUG: Server block removed ({} bytes)",
                end - start
            );

            return format!("{}{}", &content[..start], &content[end..]);
        }
    }

    crate::vlo_debug!("🔧 VLO DEBUG: No server block found");

    content.to_string()
}

pub fn load_api_actions() -> Result<HashMap<String, String>, String> {
    let file = get_project_root().join("pages/api/api.vlo");

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Loading API definitions from {}",
        file.display()
    );

    if !file.exists() {
        let error = format!("API file not found: {}", file.display());

        crate::vlo_debug!("❌ VLO DEBUG: {}", error);

        return Err(error);
    }

    let content = fs::read_to_string(&file)
        .map_err(|e| format!("Could not read {}: {}", file.display(), e))?;

    crate::vlo_debug!(
        "🔧 VLO DEBUG: API file loaded ({} bytes)",
        content.len()
    );

    let block = extract_server_block(&content)
        .ok_or_else(|| format!("No <script server> block found in {}", file.display()))?;

    let clean = block
        .trim_start_matches('\u{feff}')
        .replace('\u{a0}', " ")
        .replace('\r', "");

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Parsing API JSON ({} bytes)",
        clean.len()
    );

    let json: Value = serde_json::from_str(&clean)
        .map_err(|e| format!("Invalid JSON in {}: {}", file.display(), e))?;

    let object = json
        .as_object()
        .ok_or_else(|| "API definitions must be a JSON object".to_string())?;

    let mut actions = HashMap::new();

    for (name, value) in object {
        if let Some(sql) = value.as_str() {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: Registered API action = {}",
                name
            );

            actions.insert(name.clone(), sql.to_string());
        } else {
            crate::vlo_debug!(
                "⚠️ VLO DEBUG: Ignoring API action '{}' because value is not SQL text",
                name
            );
        }
    }

    crate::vlo_debug!(
        "✅ VLO DEBUG: Loaded {} API actions",
        actions.len()
    );

    Ok(actions)
}

pub async fn api_handler_root(
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    crate::vlo_debug!(
        "🔧 VLO DEBUG: API root request: {} /api",
        method
    );

    api_route_handler(None, None, method, query, headers, body).await
}

pub async fn api_handler_path(
    AxumPath(resource): AxumPath<String>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    crate::vlo_debug!(
        "🔧 VLO DEBUG: API path request: {} /api/{}",
        method,
        resource
    );

    api_route_handler(Some(resource), None, method, query, headers, body).await
}

pub async fn api_handler_id(
    AxumPath((resource, id)): AxumPath<(String, String)>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    crate::vlo_debug!(
        "🔧 VLO DEBUG: API ID request: {} /api/{}/{}",
        method,
        resource,
        id
    );

    api_route_handler(Some(resource), Some(id), method, query, headers, body).await
}

fn crud_operation(method: &Method) -> Option<&'static str> {
    let operation = match *method {
        Method::GET => Some("get"),
        Method::POST => Some("post"),
        Method::PUT => Some("put"),
        Method::PATCH => Some("patch"),
        Method::DELETE => Some("delete"),
        _ => None,
    };

    crate::vlo_debug!(
        "🔧 VLO DEBUG: HTTP method '{}' mapped to operation {:?}",
        method,
        operation
    );

    operation
}

fn normalize_resource(endpoint: &str) -> String {
    let value = endpoint.trim().trim_matches('/');

    for prefix in ["get_", "post_", "put_", "patch_", "delete_"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: Normalized endpoint '{}' -> '{}'",
                endpoint,
                rest
            );

            return rest.to_string();
        }
    }

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Endpoint '{}' normalized to '{}'",
        endpoint,
        value
    );

    value.to_string()
}

fn valid_identifier(value: &str) -> bool {
    let valid = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Resource identifier '{}' valid = {}",
        value,
        valid
    );

    valid
}

fn percent_decode(value: &str) -> String {
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
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Parsing form body ({} bytes)",
        body.len()
    );

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

pub async fn api_route_handler(
    mut endpoint: Option<String>,
    id: Option<String>,
    method: Method,
    mut query: HashMap<String, String>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    crate::vlo_debug!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    crate::vlo_debug!("🔧 VLO DEBUG: API request started");
    crate::vlo_debug!("🔧 VLO DEBUG: Method = {}", method);
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Initial endpoint = {:?}",
        endpoint
    );
    crate::vlo_debug!("🔧 VLO DEBUG: Initial ID = {:?}", id);
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Query parameters = {:?}",
        query
    );
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Request body length = {}",
        body.len()
    );

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Content-Type = '{}'",
        content_type
    );

    if !body.trim().is_empty() {
        crate::vlo_debug!("🔧 VLO DEBUG: Processing request body");

        if content_type.contains("application/json") {
            crate::vlo_debug!("🔧 VLO DEBUG: Body detected as JSON");

            match serde_json::from_str::<Value>(&body) {
                Ok(Value::Object(map)) => {
                    for (key, value) in map {
                        crate::vlo_debug!(
                            "🔧 VLO DEBUG: JSON parameter '{}'",
                            key
                        );

                        query.insert(key, value_to_query_string(&value));
                    }
                }

                Ok(_) => {
                    crate::vlo_debug!(
                        "⚠️ VLO DEBUG: JSON body is not an object"
                    );
                }

                Err(error) => {
                    crate::vlo_debug!(
                        "⚠️ VLO DEBUG: Failed to parse JSON body: {}",
                        error
                    );
                }
            }
        } else {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: Body detected as form data"
            );

            for (key, value) in parse_form_body(&body) {
                crate::vlo_debug!(
                    "🔧 VLO DEBUG: Form parameter '{}'",
                    key
                );

                query.insert(key, value);
            }
        }
    }

    if endpoint.is_none() {
        if let Some(action) = query.get("action").cloned() {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: Endpoint taken from query action = '{}'",
                action
            );

            endpoint = Some(action);
        }
    }

    let endpoint = match endpoint {
        Some(value) => {
            let value = value.trim().trim_matches('/').to_string();

            crate::vlo_debug!(
                "🔧 VLO DEBUG: Final endpoint = '{}'",
                value
            );

            value
        }

        None => {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: No endpoint supplied; returning API action list"
            );

            return match load_api_actions() {
                Ok(actions) => {
                    crate::vlo_debug!(
                        "✅ VLO DEBUG: Returning {} API actions",
                        actions.len()
                    );

                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "success": true,
                            "actions": actions
                        })),
                    )
                        .into_response()
                }

                Err(error) => {
                    crate::vlo_debug!(
                        "❌ VLO DEBUG: Failed to load API actions: {}",
                        error
                    );

                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "success": false,
                            "error": "Failed to load API definitions",
                            "details": error
                        })),
                    )
                        .into_response()
                }
            };
        }
    };

    let resource = normalize_resource(&endpoint);

    if !valid_identifier(&resource) {
        crate::vlo_debug!(
            "❌ VLO DEBUG: Invalid API resource '{}'",
            resource
        );

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
            crate::vlo_debug!(
                "❌ VLO DEBUG: Unsupported HTTP method '{}'",
                method
            );

            return (
                StatusCode::METHOD_NOT_ALLOWED,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Unsupported HTTP method"
                })),
            )
                .into_response();
        }
    };

    let explicit_action = [
        "get_",
        "post_",
        "put_",
        "patch_",
        "delete_",
    ]
    .iter()
    .any(|prefix| endpoint.starts_with(prefix));

    let action_name = if explicit_action {
        endpoint.clone()
    } else {
        format!("{}_{}", operation, resource)
    };

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Resource = '{}'",
        resource
    );
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Operation = '{}'",
        operation
    );
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Explicit action = {}",
        explicit_action
    );
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Final API action = '{}'",
        action_name
    );

    let actions = match load_api_actions() {
        Ok(value) => value,

        Err(error) => {
            crate::vlo_debug!(
                "❌ VLO DEBUG: Could not load API actions: {}",
                error
            );

            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Failed to load API definitions",
                    "details": error
                })),
            )
                .into_response();
        }
    };

    let mut sql = match actions.get(&action_name) {
        Some(value) => {
            crate::vlo_debug!(
                "✅ VLO DEBUG: API action '{}' found",
                action_name
            );

            crate::vlo_debug!(
                "🔧 VLO DEBUG: SQL for '{}' = {}",
                action_name,
                value
            );

            value.clone()
        }

        None => {
            crate::vlo_debug!(
                "❌ VLO DEBUG: API operation '{}' not found",
                action_name
            );

            crate::vlo_debug!(
                "🔧 VLO DEBUG: Available actions = {:?}",
                actions.keys().collect::<Vec<_>>()
            );

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
    };

    if let Some(id_value) = id.clone() {
        crate::vlo_debug!(
            "🔧 VLO DEBUG: Injecting route ID parameter = '{}'",
            id_value
        );

        query.insert("id".to_string(), id_value);
    }

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Final query parameters = {:?}",
        query
    );

    let mut params = serde_json::Map::new();

    for (key, value) in query {
        if key != "action" {
            params.insert(key, query_string_to_value(&value));
        }
    }

    crate::vlo_debug!(
        "🔧 VLO DEBUG: SQL parameters = {:?}",
        params
    );

    if id.is_some()
        && operation == "get"
        && !sql.contains("{{id}}")
        && !sql.contains("{id}")
    {
        crate::vlo_debug!(
            "🔧 VLO DEBUG: GET-by-ID requested without ID placeholder; modifying SQL"
        );

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

        crate::vlo_debug!(
            "🔧 VLO DEBUG: Modified GET-by-ID SQL = {}",
            sql
        );
    }

    let pool = match DB_POOL.get() {
        Some(p) => {
            crate::vlo_debug!("✅ VLO DEBUG: Database pool found");

            match p {
                DbPool::Sqlite(_) => {
                    crate::vlo_debug!("🔧 VLO DEBUG: Active DB driver = SQLite");
                }

                DbPool::Postgres(_) => {
                    crate::vlo_debug!("🔧 VLO DEBUG: Active DB driver = PostgreSQL");
                }

                DbPool::MySql(_) => {
                    crate::vlo_debug!("🔧 VLO DEBUG: Active DB driver = MySQL");
                }
            }

            p
        }

        None => {
            crate::vlo_debug!(
                "❌ VLO DEBUG: DB_POOL is not initialized"
            );

            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Database not configured"
                })),
            )
                .into_response();
        }
    };

    let action_type =
        if action_name.starts_with("put") || method == Method::PUT {
            "updated"
        } else if action_name.starts_with("delete")
            || method == Method::DELETE
        {
            "deleted"
        } else {
            "created"
        };

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Action type = '{}'",
        action_type
    );

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Executing SQL = {}",
        sql
    );

    match execute_api_sql(pool, &sql, &params).await {
        Ok(data) => {
            crate::vlo_debug!(
                "✅ VLO DEBUG: SQL execution successful"
            );

            crate::vlo_debug!(
                "🔧 VLO DEBUG: SQL result = {}",
                data
            );

            if method != Method::GET {
                let redirect_url = format!(
                    "/{}?status=success&action={}",
                    resource,
                    action_type
                );

                crate::vlo_debug!(
                    "🔧 VLO DEBUG: Non-GET request; redirecting to {}",
                    redirect_url
                );

                return Redirect::to(&redirect_url).into_response();
            }

            crate::vlo_debug!(
                "🔧 VLO DEBUG: Returning GET JSON response"
            );

            (
                StatusCode::OK,
                Json(data),
            )
                .into_response()
        }

        Err(error) => {
            eprintln!(
                "❌ [VLO API] {} {} -> SQL error: {}",
                method,
                action_name,
                error
            );

            crate::vlo_debug!(
                "❌ VLO DEBUG: SQL execution failed for '{}'",
                action_name
            );

            if method != Method::GET {
                let redirect_url = format!(
                    "/{}?status=error&action={}",
                    resource,
                    action_type
                );

                crate::vlo_debug!(
                    "🔧 VLO DEBUG: Redirecting failed mutation to {}",
                    redirect_url
                );

                return Redirect::to(&redirect_url).into_response();
            }

            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": "SQL Execution Error",
                    "details": error,
                    "action": action_name
                })),
            )
                .into_response()
        }
    }
}

pub enum QueryParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Json(serde_json::Value),
}

fn value_to_any_param(value: &Value) -> QueryParam {
    match value {
        Value::Null => QueryParam::Null,

        Value::Bool(b) => QueryParam::Bool(*b),

        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                QueryParam::Int(i)
            } else if let Some(f) = n.as_f64() {
                QueryParam::Float(f)
            } else {
                QueryParam::Text(n.to_string())
            }
        }

        Value::String(s) => QueryParam::Text(s.clone()),

        _ => QueryParam::Json(value.clone()),
    }
}

fn prepare_sql(
    sql: &str,
    params: &serde_json::Map<String, Value>,
    pool: &DbPool,
) -> Result<(String, Vec<QueryParam>), String> {
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Preparing SQL = {}",
        sql
    );

    crate::vlo_debug!(
        "🔧 VLO DEBUG: SQL parameters = {:?}",
        params
    );

    let mut values = Vec::new();
    let mut param_index = 1;
    let is_postgres = matches!(pool, DbPool::Postgres(_));

    let prepared = crate::state::SQL_PARAM_RE
        .replace_all(sql, |caps: &regex::Captures| {
            let key = &caps[1];

            crate::vlo_debug!(
                "🔧 VLO DEBUG: Resolving SQL parameter '{{{{{}}}}}'",
                key
            );

            match params.get(key) {
                Some(value) => {
                    crate::vlo_debug!(
                        "🔧 VLO DEBUG: Parameter '{}' resolved to {}",
                        key,
                        value
                    );

                    let placeholder = if is_postgres {
                        let ph = format!("${}", param_index);
                        param_index += 1;
                        ph
                    } else {
                        param_index += 1;
                        "?".to_string()
                    };

                    values.push(value_to_any_param(value));

                    placeholder
                }

                None => {
                    crate::vlo_debug!(
                        "⚠️ VLO DEBUG: Parameter '{}' missing; binding NULL",
                        key
                    );

                    let placeholder = if is_postgres {
                        let ph = format!("${}", param_index);
                        param_index += 1;
                        ph
                    } else {
                        param_index += 1;
                        "?".to_string()
                    };

                    values.push(QueryParam::Null);

                    placeholder
                }
            }
        })
        .into_owned();

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Prepared SQL = {}",
        prepared
    );

    if prepared.contains('{') || prepared.contains('}') {
        crate::vlo_debug!(
            "❌ VLO DEBUG: Unresolved SQL parameter detected"
        );

        return Err(format!(
            "Unresolved parameter in SQL: {}",
            prepared
        ));
    }

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Prepared {} SQL bind values",
        values.len()
    );

    Ok((prepared, values))
}

macro_rules! convert_row_to_json {
    ($row:expr) => {{
        let mut map = serde_json::Map::new();

        crate::vlo_debug!(
            "🔧 VLO DEBUG: Converting database row with {} columns",
            $row.columns().len()
        );

        for (i, column) in $row.columns().iter().enumerate() {
            let name = column.name().to_string();

            crate::vlo_debug!(
                "🔧 VLO DEBUG: Reading column {} = '{}'",
                i,
                name
            );

            let is_null = $row
                .try_get_raw(i)
                .map(|r| r.is_null())
                .unwrap_or(true);

            let val: serde_json::Value = if is_null {
                serde_json::Value::Null
            } else {
                let type_name =
                    column.type_info().name().to_lowercase();

                crate::vlo_debug!(
                    "🔧 VLO DEBUG: Column '{}' type = '{}'",
                    name,
                    type_name
                );

                if type_name.contains("int") {
                    $row.try_get::<i64, _>(i)
                        .or_else(|_| {
                            $row.try_get::<i32, _>(i)
                                .map(|v| v as i64)
                        })
                        .map(|v| serde_json::Value::Number(v.into()))
                        .unwrap_or(serde_json::Value::Null)
                } else if type_name.contains("bool") {
                    $row.try_get::<bool, _>(i)
                        .map(serde_json::Value::Bool)
                        .unwrap_or(serde_json::Value::Null)
                } else if type_name.contains("float")
                    || type_name.contains("double")
                    || type_name.contains("real")
                    || type_name.contains("numeric")
                    || type_name.contains("decimal")
                {
                    $row.try_get::<f64, _>(i)
                        .ok()
                        .and_then(|v| {
                            serde_json::Number::from_f64(v)
                                .map(serde_json::Value::Number)
                        })
                        .unwrap_or(serde_json::Value::Null)
                } else if type_name.contains("json") {
                    $row.try_get::<sqlx::types::Json<serde_json::Value>, _>(i)
                        .map(|j| j.0)
                        .unwrap_or(serde_json::Value::Null)
                } else {
                    $row.try_get::<String, _>(i)
                        .map(serde_json::Value::String)
                        .unwrap_or_else(|_| {
                            $row.try_get::<Vec<u8>, _>(i)
                                .map(|v| {
                                    serde_json::Value::String(
                                        format!("blob {}b", v.len()),
                                    )
                                })
                                .unwrap_or(serde_json::Value::Null)
                        })
                }
            };

            crate::vlo_debug!(
                "🔧 VLO DEBUG: Column '{}' converted to JSON = {}",
                name,
                val
            );

            map.insert(name, val);
        }

        serde_json::Value::Object(map)
    }};
}

fn sqlite_row_to_json(
    row: &sqlx::sqlite::SqliteRow,
) -> serde_json::Value {
    convert_row_to_json!(row)
}

fn pg_row_to_json(
    row: &sqlx::postgres::PgRow,
) -> serde_json::Value {
    convert_row_to_json!(row)
}

fn mysql_row_to_json(
    row: &sqlx::mysql::MySqlRow,
) -> serde_json::Value {
    convert_row_to_json!(row)
}

pub async fn execute_api_sql(
    pool: &DbPool,
    sql: &str,
    params: &serde_json::Map<String, Value>,
) -> Result<Value, String> {
    crate::vlo_debug!("────────────────────────────────────────");
    crate::vlo_debug!("🔧 VLO DEBUG: execute_api_sql() started");
    crate::vlo_debug!("🔧 VLO DEBUG: SQL = {}", sql);
    crate::vlo_debug!("🔧 VLO DEBUG: Params = {:?}", params);

    let mut last_data = None;
    let mut affected_rows = 0u64;

    macro_rules! exec_db {
        ($pool:expr, $to_json:ident) => {{
            crate::vlo_debug!(
                "🔧 VLO DEBUG: Beginning database transaction"
            );

            let mut tx = $pool
                .begin()
                .await
                .map_err(|e| {
                    crate::vlo_debug!(
                        "❌ VLO DEBUG: Failed to begin transaction: {}",
                        e
                    );
                    e.to_string()
                })?;

            for statement in sql.split(';') {
                let statement = statement.trim();

                if statement.is_empty() {
                    continue;
                }

                crate::vlo_debug!(
                    "🔧 VLO DEBUG: Processing SQL statement = {}",
                    statement
                );

                let (prepared_sql, values) =
                    prepare_sql(statement, params, pool)?;

                let upper = prepared_sql
                    .trim_start()
                    .to_uppercase();

                let is_select = upper.starts_with("SELECT")
                    || upper.starts_with("PRAGMA")
                    || upper.starts_with("WITH");

                crate::vlo_debug!(
                    "🔧 VLO DEBUG: Statement type = {}",
                    if is_select {
                        "QUERY"
                    } else {
                        "MUTATION"
                    }
                );

                let mut query = sqlx::query(&prepared_sql);

                crate::vlo_debug!(
                    "🔧 VLO DEBUG: Binding {} parameters",
                    values.len()
                );

                for p in &values {
                    match p {
                        QueryParam::Null => {
                            crate::vlo_debug!(
                                "🔧 VLO DEBUG: Binding NULL"
                            );

                            query =
                                query.bind(Option::<String>::None)
                        }

                        QueryParam::Bool(b) => {
                            crate::vlo_debug!(
                                "🔧 VLO DEBUG: Binding BOOL = {}",
                                b
                            );

                            query = query.bind(*b)
                        }

                        QueryParam::Int(i) => {
                            crate::vlo_debug!(
                                "🔧 VLO DEBUG: Binding INT = {}",
                                i
                            );

                            query = query.bind(*i)
                        }

                        QueryParam::Float(f) => {
                            crate::vlo_debug!(
                                "🔧 VLO DEBUG: Binding FLOAT = {}",
                                f
                            );

                            query = query.bind(*f)
                        }

                        QueryParam::Text(s) => {
                            crate::vlo_debug!(
                                "🔧 VLO DEBUG: Binding TEXT = '{}'",
                                s
                            );

                            query = query.bind(s.clone())
                        }

                        QueryParam::Json(j) => {
                            crate::vlo_debug!(
                                "🔧 VLO DEBUG: Binding JSON = {}",
                                j
                            );

                            query =
                                query.bind(sqlx::types::Json(j.clone()))
                        }
                    }
                }

                if is_select {
                    crate::vlo_debug!(
                        "🔧 VLO DEBUG: Executing SELECT query"
                    );

                    let rows = query
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(|e| {
                            crate::vlo_debug!(
                                "❌ VLO DEBUG: SELECT failed: {}",
                                e
                            );

                            e.to_string()
                        })?;

                    crate::vlo_debug!(
                        "✅ VLO DEBUG: SELECT returned {} rows",
                        rows.len()
                    );

                    let mut data = Vec::new();

                    for (index, row) in rows.iter().enumerate() {
                        crate::vlo_debug!(
                            "🔧 VLO DEBUG: Converting row {}",
                            index + 1
                        );

                        data.push($to_json(row));
                    }

                    crate::vlo_debug!(
                        "🔧 VLO DEBUG: Converted {} rows to JSON",
                        data.len()
                    );

                    last_data = Some(data);
                } else {
                    crate::vlo_debug!(
                        "🔧 VLO DEBUG: Executing mutation query"
                    );
                    let res = query
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| {
                            crate::vlo_debug!(
                                "❌ VLO DEBUG: Mutation failed: {}",
                                e
                            );
                            e.to_string()
                        })?;

                    let rows = res.rows_affected();

                    crate::vlo_debug!(
                        "✅ VLO DEBUG: Mutation affected {} rows",
                        rows
                    );

                    affected_rows += rows;
                }
            }

            crate::vlo_debug!(
                "🔧 VLO DEBUG: Committing database transaction"
            );

            tx.commit().await.map_err(|e| {
                crate::vlo_debug!(
                    "❌ VLO DEBUG: Transaction commit failed: {}",
                    e
                );

                e.to_string()
            })?;

            crate::vlo_debug!(
                "✅ VLO DEBUG: Database transaction committed"
            );
        }};
    }

    match pool {
        DbPool::Sqlite(p) => {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: execute_api_sql using SQLite"
            );

            exec_db!(p, sqlite_row_to_json)
        }

        DbPool::Postgres(p) => {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: execute_api_sql using PostgreSQL"
            );

            exec_db!(p, pg_row_to_json)
        }

        DbPool::MySql(p) => {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: execute_api_sql using MySQL"
            );

            exec_db!(p, mysql_row_to_json)
        }
    }

    let result = if let Some(data) = last_data {
        crate::vlo_debug!(
            "✅ VLO DEBUG: Returning {} query rows",
            data.len()
        );

        serde_json::json!({
            "data": data,
            "affected_rows": affected_rows
        })
    } else {
        crate::vlo_debug!(
            "✅ VLO DEBUG: Returning mutation result: affected_rows={}",
            affected_rows
        );

        serde_json::json!({
            "success": true,
            "affected_rows": affected_rows
        })
    };

    crate::vlo_debug!(
        "🔧 VLO DEBUG: execute_api_sql() finished"
    );
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Final result = {}",
        result
    );

    Ok(result)
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
        return Value::String(value.to_string());
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
        if let Some(number) =
            serde_json::Number::from_f64(float)
        {
            return Value::Number(number);
        }
    }

    Value::String(value.to_string())
}