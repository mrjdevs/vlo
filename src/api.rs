use crate::{
    database::{DbPool, DB_POOL},
    state::get_project_root,
    vlo_debug,
};
use axum::{
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Json, Redirect},
};
use serde_json::Value;
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::{collections::HashMap, fs};

// ============================================================
// 10. VLO SERVER BLOCK
// ============================================================

pub fn extract_server_block(content: &str) -> Option<String> {
    let start = content.find("<script server>")?;
    let rest = &content[start + 15..];
    let end = rest.find("</script>")?;

    Some(rest[..end].trim().to_string())
}

pub fn strip_server_block(content: &str) -> String {
    if let Some(start) = content.find("<script server>") {
        if let Some(end) = content[start..].find("</script>") {
            let end = start + end + "</script>".len();

            return format!(
                "{}{}",
                &content[..start],
                &content[end..]
            );
        }
    }

    content.to_string()
}

// ============================================================
// 11. VLO API DEFINITION LOADER
// ============================================================

pub fn load_api_actions() -> Result<HashMap<String, String>, String> {
    let file = get_project_root().join("pages/api/api.vlo");

    if !file.exists() {
        return Err(format!(
            "API file not found: {}",
            file.display()
        ));
    }

    let content = fs::read_to_string(&file)
        .map_err(|e| {
            format!(
                "Could not read {}: {}",
                file.display(),
                e
            )
        })?;

    let block = extract_server_block(&content)
        .ok_or_else(|| {
            format!(
                "No <script server> block found in {}",
                file.display()
            )
        })?;

    let clean = block
        .trim_start_matches('\u{feff}')
        .replace('\u{a0}', " ")
        .replace('\r', "");

    let json: Value = serde_json::from_str(&clean)
        .map_err(|e| {
            format!(
                "Invalid JSON in {}: {}",
                file.display(),
                e
            )
        })?;

    let object = json
        .as_object()
        .ok_or_else(|| {
            "API definitions must be a JSON object".to_string()
        })?;

    let mut actions = HashMap::new();

    for (name, value) in object {
        if let Some(sql) = value.as_str() {
            actions.insert(name.clone(), sql.to_string());
        }
    }

    Ok(actions)
}

// ============================================================
// 12. VLO API ENGINE & 13. VLO SQL ENGINE
// ============================================================

pub async fn api_handler_root(
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    api_route_handler(
        None,
        None,
        method,
        query,
        headers,
        body,
    )
    .await
}

pub async fn api_handler_path(
    AxumPath(resource): AxumPath<String>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    api_route_handler(
        Some(resource),
        None,
        method,
        query,
        headers,
        body,
    )
    .await
}

pub async fn api_handler_id(
    AxumPath((resource, id)): AxumPath<(String, String)>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    api_route_handler(
        Some(resource),
        Some(id),
        method,
        query,
        headers,
        body,
    )
    .await
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

    for prefix in [
        "get_",
        "post_",
        "put_",
        "patch_",
        "delete_",
    ] {
        if let Some(rest) = value.strip_prefix(prefix) {
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

            let key = percent_decode(
                pair.next().unwrap_or(""),
            );

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

pub async fn api_route_handler(
    mut endpoint: Option<String>,
    id: Option<String>,
    method: Method,
    mut query: HashMap<String, String>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if !body.trim().is_empty() {
        if content_type.contains("application/json") {
            if let Ok(Value::Object(map)) =
                serde_json::from_str::<Value>(&body)
            {
                for (key, value) in map {
                    query.insert(
                        key,
                        value_to_query_string(&value),
                    );
                }
            }
        } else if content_type
            .contains("application/x-www-form-urlencoded")
        {
            for (key, value) in parse_form_body(&body) {
                query.insert(key, value);
            }
        }
    }

    if endpoint.is_none() {
        if let Some(action) = query.get("action").cloned() {
            endpoint = Some(action);
        }
    }

    let endpoint = match endpoint {
        Some(value) => value.trim().trim_matches('/').to_string(),

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

    let mut params = serde_json::Map::new();

    for (key, value) in query {
        if key != "action" {
            params.insert(
                key,
                query_string_to_value(&value),
            );
        }
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
            let trimmed = sql
                .trim_end_matches(';')
                .trim_end();

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

    match execute_api_sql(pool, &sql, &params).await {
        Ok(data) => {
            vlo_debug!(
                "✅ [VLO API] {} {} -> success: {:?}",
                method,
                action_name,
                data
            );

            if method == Method::POST
                && resource == "products"
            {
                vlo_debug!(
                    "↪️ [VLO API] POST products completed -> redirecting to /products"
                );

                return Redirect::to("/products")
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

            vlo_debug!(
                "❌ [VLO API] SQL failure for {}: {}",
                action_name,
                error
            );

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
    let mut values = Vec::new();
    let mut param_index = 1;
    let is_postgres = matches!(pool, DbPool::Postgres(_));

    let prepared = crate::state::SQL_PARAM_RE
        .replace_all(sql, |caps: &regex::Captures| {
            let key = &caps[1];
            match params.get(key) {
                Some(value) => {
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

    if prepared.contains('{') || prepared.contains('}') {
        return Err(format!("Unresolved parameter in SQL: {}", prepared));
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

            let val: serde_json::Value = if is_null {
                serde_json::Value::Null
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
                            serde_json::Value::Number(v.into())
                        })
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

            map.insert(name, val);
        }

        serde_json::Value::Object(map)
    }};
}

fn sqlite_row_to_json(row: &sqlx::sqlite::SqliteRow) -> serde_json::Value {
    convert_row_to_json!(row)
}

fn pg_row_to_json(row: &sqlx::postgres::PgRow) -> serde_json::Value {
    convert_row_to_json!(row)
}

fn mysql_row_to_json(row: &sqlx::mysql::MySqlRow) -> serde_json::Value {
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
                    prepare_sql(statement, params, pool)?;

                let upper = prepared_sql.trim_start().to_uppercase();
                let is_select = upper.starts_with("SELECT")
                    || upper.starts_with("PRAGMA")
                    || upper.starts_with("WITH");

                let mut query = sqlx::query(&prepared_sql);

                for p in &values {
                    match p {
                        QueryParam::Null => {
                            query = query.bind(Option::<String>::None)
                        }
                        QueryParam::Bool(b) => query = query.bind(*b),
                        QueryParam::Int(i) => query = query.bind(*i),
                        QueryParam::Float(f) => query = query.bind(*f),
                        QueryParam::Text(s) => query = query.bind(s.clone()),
                        QueryParam::Json(j) => {
                            query = query.bind(sqlx::types::Json(j.clone()))
                        }
                    }
                }

                if is_select {
                    let rows = query
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(|e| e.to_string())?;

                    let mut data = Vec::new();
                    for row in rows {
                        data.push($to_json(&row));
                    }
                    last_data = Some(data);
                } else {
                    let res = query
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| e.to_string())?;

                    affected_rows += res.rows_affected();
                }
            }

            tx.commit().await.map_err(|e| e.to_string())?;
        }};
    }

    match pool {
        DbPool::Sqlite(p) => exec_db!(p, sqlite_row_to_json),
        DbPool::Postgres(p) => exec_db!(p, pg_row_to_json),
        DbPool::MySql(p) => exec_db!(p, mysql_row_to_json),
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
        if let Some(number) = serde_json::Number::from_f64(float) {
            return Value::Number(number);
        }
    }
    Value::String(value.to_string())
}