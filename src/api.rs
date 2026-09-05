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
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

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
        return Err(format!("API file not found: {}", file.display()));
    }

    let content = fs::read_to_string(&file)
        .map_err(|e| format!("Could not read {}: {}", file.display(), e))?;

    let block = extract_server_block(&content)
        .ok_or_else(|| format!("No <script server> block found in {}", file.display()))?;

    let clean = block
        .trim_start_matches('\u{feff}')
        .replace('\u{a0}', " ")
        .replace('\r', "");

    let json: Value = serde_json::from_str(&clean)
        .map_err(|e| format!("Invalid JSON in {}: {}", file.display(), e))?;

    let object = json
        .as_object()
        .ok_or_else(|| "API definitions must be a JSON object".to_string())?;

    let mut actions = HashMap::new();

    for (name, value) in object {
        if let Some(sql) = value.as_str() {
            crate::vlo_debug!("🔧 VLO DEBUG: Registered API action = {}", name);
            actions.insert(name.clone(), sql.to_string());
        }
    }

    crate::vlo_debug!("✅ VLO DEBUG: Loaded {} API actions", actions.len());
    Ok(actions)
}

pub async fn api_handler_root(req: Request) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => {
            api_route_handler(None, None, method, query)
                .await
                .into_response()
        }
        Err(response) => response,
    }
}

pub async fn api_handler_path(
    AxumPath(resource): AxumPath<String>,
    req: Request,
) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => {
            api_route_handler(Some(resource), None, method, query)
                .await
                .into_response()
        }
        Err(response) => response,
    }
}

pub async fn api_handler_id(
    AxumPath((resource, id)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    match prepare_api_request(req).await {
        Ok((method, query)) => {
            api_route_handler(Some(resource), Some(id), method, query)
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
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let mut query = req
        .uri()
        .query()
        .map(parse_form_body)
        .unwrap_or_default();

    if content_type.starts_with("multipart/form-data") {
        let mut multipart = match Multipart::from_request(req, &()).await {
            Ok(multipart) => multipart,
            Err(error) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Invalid multipart request",
                        "details": error.to_string()
                    })),
                )
                    .into_response());
            }
        };

        if let Err(response) = parse_multipart_body(&mut multipart, &mut query).await {
            return Err(response);
        }
    } else {
        let body = match Bytes::from_request(req, &()).await {
            Ok(body) => body,
            Err(error) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Invalid request body",
                        "details": error.to_string()
                    })),
                )
                    .into_response());
            }
        };

        if !body.is_empty() {
            if content_type.contains("application/json") {
                match serde_json::from_slice::<Value>(&body) {
                    Ok(Value::Object(object)) => {
                        for (key, value) in object {
                            query.insert(key, value_to_query_string(&value));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "success": false,
                                "error": "Invalid JSON",
                                "details": error.to_string()
                            })),
                        )
                            .into_response());
                    }
                }
            } else if content_type.contains("application/x-www-form-urlencoded") {
                let body_string = String::from_utf8_lossy(&body);
                let form_values = parse_form_body(&body_string);

                for (key, value) in form_values {
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
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Invalid multipart request",
                        "details": error.to_string()
                    })),
                )
                    .into_response());
            }
        };

        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(|name| name.to_string());

        let bytes = match field.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Failed to read uploaded file",
                        "details": error.to_string()
                    })),
                )
                    .into_response());
            }
        };

        if bytes.len() as u64 > max_size {
            let max_mb = max_size / (1024 * 1024);

            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({
                    "success": false,
                    "error": "File too large",
                    "details": format!(
                        "Maximum allowed upload size is {} MB",
                        max_mb
                    )
                })),
            )
                .into_response());
        }

        if let Some(original_name) = file_name {
            let upload_name = generate_upload_name(&original_name);

            if let Err(error) = crate::files::save(&upload_name, &bytes) {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Failed to save uploaded file",
                        "details": error.to_string()
                    })),
                )
                    .into_response());
            }

            query.insert(
                name,
                format!("/uploads/{}", upload_name),
            );
        } else {
            let value = String::from_utf8_lossy(&bytes).to_string();
            query.insert(name, value);
        }
    }

    Ok(())
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

    for prefix in [
        "get_",
        "post_",
        "put_",
        "patch_",
        "delete_",
    ] {
        if let Some(rest) = value.strip_prefix(prefix) {
            crate::vlo_debug!(
                "🔧 VLO DEBUG: Normalized endpoint '{}' -> '{}'",
                endpoint,
                rest
            );

            return rest.to_string();
        }
    }

    value.to_string()
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
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

            if let (Some(a), Some(b)) =
                (h(bytes[i + 1]), h(bytes[i + 2]))
            {
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

            Some((
                key,
                percent_decode(pair.next().unwrap_or("")),
            ))
        })
        .collect()
}


fn generate_upload_name(original: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();

    let extension = Path::new(original)
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| v.to_ascii_lowercase())
        .filter(|v| !v.is_empty());

    match extension {
        Some(ext) => format!(
            "vlo_{}_{}.{}",
            timestamp,
            std::process::id(),
            ext
        ),

        None => format!(
            "vlo_{}_{}",
            timestamp,
            std::process::id()
        ),
    }
}

pub async fn api_route_handler(
    mut endpoint: Option<String>,
    id: Option<String>,
    method: Method,
    mut query: HashMap<String, String>,
) -> impl IntoResponse {
    crate::vlo_debug!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    crate::vlo_debug!("🔧 VLO DEBUG: API request started");
    crate::vlo_debug!("🔧 VLO DEBUG: Method = {}", method);
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Initial endpoint = {:?}",
        endpoint
    );
    crate::vlo_debug!(
        "🔧 VLO DEBUG: Initial ID = {:?}",
        id
    );

    if endpoint.is_none() {
        if let Some(action) = query.get("action").cloned() {
            endpoint = Some(action);
        }
    }

    let endpoint = match endpoint {
        Some(value) => {
            value.trim().trim_matches('/').to_string()
        }

        None => {
            return match load_api_actions() {
                Ok(actions) => (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "actions": actions
                    })),
                )
                    .into_response(),

                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Failed to load API definitions",
                        "details": error
                    })),
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
        "🔧 VLO DEBUG: Resource = '{}', operation = '{}', action = '{}'",
        resource,
        operation,
        action_name
    );

    let actions = match load_api_actions() {
        Ok(value) => value,

        Err(error) => {
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
        Some(value) => value.clone(),

        None => {
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
        query.insert("id".to_string(), id_value);
    }

    if id.is_some()
        && operation == "get"
        && !sql.contains("{{id}}")
        && !sql.contains("{id}")
    {
        let upper = sql.to_uppercase();

        if let Some(pos) = upper.find(" ORDER BY ") {
            let before = sql[..pos].trim_end();
            let order = &sql[pos..];

            sql = if before
                .to_uppercase()
                .contains(" WHERE ")
            {
                format!(
                    "{} AND id = {{id}}{}",
                    before,
                    order
                )
            } else {
                format!(
                    "{} WHERE id = {{id}}{}",
                    before,
                    order
                )
            };
        } else {
            let trimmed =
                sql.trim_end_matches(';').trim();

            sql = if trimmed
                .to_uppercase()
                .contains(" WHERE ")
            {
                format!(
                    "{} AND id = {{id}}",
                    trimmed
                )
            } else {
                format!(
                    "{} WHERE id = {{id}}",
                    trimmed
                )
            };
        }
    }

    let mut params = serde_json::Map::new();

    for (key, value) in query {
        if key != "action" {
            params.insert(
                key,
                query_string_to_value(&value),
            );
        }
    }

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Final SQL parameters = {:?}",
        params
    );

    let pool = match DB_POOL.get() {
        Some(p) => p,

        None => {
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
        if action_name.starts_with("put")
            || action_name.starts_with("patch")
            || method == Method::PUT
            || method == Method::PATCH
        {
            "updated"
        } else if action_name.starts_with("delete")
            || method == Method::DELETE
        {
            "deleted"
        } else {
            "created"
        };

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Executing SQL = {}",
        sql
    );

    match execute_api_sql(pool, &sql, &params).await {
        Ok(data) => {
            crate::vlo_debug!(
                "✅ VLO DEBUG: SQL execution successful: {}",
                data
            );

            if method != Method::GET {
                let redirect_url = format!(
                    "/{}?status=success&action={}",
                    resource,
                    action_type
                );

                return Redirect::to(&redirect_url)
                    .into_response();
            }

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

            if method != Method::GET {
                let redirect_url = format!(
                    "/{}?status=error&action={}",
                    resource,
                    action_type
                );

                return Redirect::to(&redirect_url)
                    .into_response();
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
    Json(Value),
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
    let mut values = Vec::new();
    let mut param_index = 1;
    let is_postgres =
        matches!(pool, DbPool::Postgres(_));

    let prepared = crate::state::SQL_PARAM_RE
        .replace_all(
            sql,
            |caps: &regex::Captures| {
                let key = &caps[1];

                let placeholder = if is_postgres {
                    let value =
                        format!("${}", param_index);
                    param_index += 1;
                    value
                } else {
                    param_index += 1;
                    "?".to_string()
                };

                values.push(match params.get(key) {
                    Some(value) => {
                        value_to_any_param(value)
                    }
                    None => QueryParam::Null,
                });

                placeholder
            },
        )
        .into_owned();

    if prepared.contains('{')
        || prepared.contains('}')
    {
        return Err(format!(
            "Unresolved parameter in SQL: {}",
            prepared
        ));
    }

    Ok((prepared, values))
}

macro_rules! convert_row_to_json {
    ($row:expr) => {{
        let mut map = serde_json::Map::new();

        for (i, column) in $row.columns().iter().enumerate() {
            let name = column.name().to_string();

            let is_null = $row
                .try_get_raw(i)
                .map(|r| r.is_null())
                .unwrap_or(true);

            let val = if is_null {
                Value::Null
            } else {
                let type_name =
                    column.type_info().name().to_lowercase();

                if type_name.contains("int") {
                    $row.try_get::<i64, _>(i)
                        .or_else(|_| {
                            $row.try_get::<i32, _>(i)
                                .map(|v| v as i64)
                        })
                        .map(|v| {
                            Value::Number(v.into())
                        })
                        .unwrap_or(Value::Null)
                } else if type_name.contains("bool") {
                    $row.try_get::<bool, _>(i)
                        .map(Value::Bool)
                        .unwrap_or(Value::Null)
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
                                .map(Value::Number)
                        })
                        .unwrap_or(Value::Null)
                } else if type_name.contains("json") {
                    $row.try_get::<sqlx::types::Json<Value>, _>(i)
                        .map(|j| j.0)
                        .unwrap_or(Value::Null)
                } else {
                    $row.try_get::<String, _>(i)
                        .map(Value::String)
                        .unwrap_or_else(|_| {
                            $row.try_get::<Vec<u8>, _>(i)
                                .map(|v| {
                                    Value::String(
                                        format!("blob {}b", v.len()),
                                    )
                                })
                                .unwrap_or(Value::Null)
                        })
                }
            };

            map.insert(name, val);
        }

        Value::Object(map)
    }};
}

fn sqlite_row_to_json(
    row: &sqlx::sqlite::SqliteRow,
) -> Value {
    convert_row_to_json!(row)
}

fn pg_row_to_json(
    row: &sqlx::postgres::PgRow,
) -> Value {
    convert_row_to_json!(row)
}

fn mysql_row_to_json(
    row: &sqlx::mysql::MySqlRow,
) -> Value {
    convert_row_to_json!(row)
}

pub async fn execute_api_sql(
    pool: &DbPool,
    sql: &str,
    params: &serde_json::Map<String, Value>,
) -> Result<Value, String> {
    let mut last_data = None;
    let mut affected_rows = 0u64;

    macro_rules! exec_db {
        ($pool:expr, $to_json:ident) => {{
            let mut tx = $pool
                .begin()
                .await
                .map_err(|e| e.to_string())?;

            for statement in sql.split(';') {
                let statement = statement.trim();

                if statement.is_empty() {
                    continue;
                }

                let (prepared_sql, values) =
                    prepare_sql(
                        statement,
                        params,
                        pool,
                    )?;

                let upper = prepared_sql
                    .trim_start()
                    .to_uppercase();

                let is_select =
                    upper.starts_with("SELECT")
                        || upper.starts_with("PRAGMA")
                        || upper.starts_with("WITH");

                let mut query =
                    sqlx::query(&prepared_sql);

                for value in &values {
                    query = match value {
                        QueryParam::Null => {
                            query.bind(
                                Option::<String>::None,
                            )
                        }

                        QueryParam::Bool(v) =>
                            query.bind(*v),

                        QueryParam::Int(v) =>
                            query.bind(*v),

                        QueryParam::Float(v) =>
                            query.bind(*v),

                        QueryParam::Text(v) =>
                            query.bind(v.clone()),

                        QueryParam::Json(v) =>
                            query.bind(
                                sqlx::types::Json(
                                    v.clone(),
                                ),
                            ),
                    };
                }

                if is_select {
                    let rows = query
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(|e| e.to_string())?;

                    let mut data =
                        Vec::with_capacity(rows.len());

                    for row in &rows {
                        data.push($to_json(row));
                    }

                    last_data = Some(data);
                } else {
                    let result = query
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| e.to_string())?;

                    affected_rows +=
                        result.rows_affected();
                }
            }

            tx.commit()
                .await
                .map_err(|e| e.to_string())?;
        }};
    }

    match pool {
        DbPool::Sqlite(p) =>
            exec_db!(p, sqlite_row_to_json),

        DbPool::Postgres(p) =>
            exec_db!(p, pg_row_to_json),

        DbPool::MySql(p) =>
            exec_db!(p, mysql_row_to_json),
    }

    if let Some(data) = last_data {
        Ok(serde_json::json!({
            "data": data,
            "affected_rows": affected_rows
        }))
    } else {
        Ok(serde_json::json!({
            "success": true,
            "affected_rows": affected_rows
        }))
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