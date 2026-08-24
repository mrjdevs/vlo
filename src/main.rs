use async_stream::stream;
use axum::{
    extract::{Path as AxumPath, Query},
    http::{Method, StatusCode},
    response::{
        sse::{Event, KeepAlive},
        Html, IntoResponse, Json, Sse,
    },
    routing::get,
    Router,
};
use clap::{Parser, Subcommand};
use futures_util::stream::Stream;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Result};
use serde_json::Value;
use std::{
    collections::HashMap,
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc::channel, Arc, Mutex},
};
use tokio::sync::broadcast;
use tower_http::services::ServeDir;

#[derive(Parser)]
#[command(name = "vlo", version, about = "VLO Web Framework")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Dev,
    Build,
    Deploy {
        #[arg(short, long, default_value = "netlify")]
        provider: String,
    },
}

#[tokio::main]
async fn main() {
    match Cli::parse().command {
        Commands::Dev => dev().await,
        Commands::Build => build(),
        Commands::Deploy { provider } => deploy(&provider).await,
    }
}

fn get_project_root() -> PathBuf {
    let mut starts = Vec::new();
    if let Ok(dir) = std::env::current_dir() {
        starts.push(dir);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            starts.push(parent.to_path_buf());
        }
    }
    starts.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    for start in starts {
        let mut dir = start;
        loop {
            if dir.join("pages").exists() {
                return dir;
            }
            if !dir.pop() {
                break;
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn get_db_conn() -> Result<Connection> {
    let root = get_project_root();
    let conn = Connection::open(root.join("vlo_app.db"))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;",
    )?;
    let schema = root.join("schema.sql");
    if schema.exists() {
        let sql = fs::read_to_string(schema)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let sql = sql
            .lines()
            .filter(|line| {
                let t = line.trim().to_uppercase();
                !t.starts_with("CREATE DATABASE") && !t.starts_with("USE ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        conn.execute_batch(&sql)?;
    }
    Ok(conn)
}

fn extract_server_block(content: &str) -> Option<String> {
    let start = content.find("<script server>")?;
    let rest = &content[start + 15..];
    let end = rest.find("</script>")?;
    Some(rest[..end].trim().to_string())
}

fn strip_server_block(content: &str) -> String {
    if let Some(start) = content.find("<script server>") {
        if let Some(end) = content[start..].find("</script>") {
            let end = start + end + "</script>".len();
            return format!("{}{}", &content[..start], &content[end..]);
        }
    }
    content.to_string()
}

fn load_api_actions() -> Result<HashMap<String, String>, String> {
    let root = get_project_root();
    let file = root.join("pages").join("api").join("api.vlo");
    println!("🔎 [VLO API] Loading: {}", file.display());
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
            actions.insert(name.clone(), sql.to_string());
        }
    }
    println!("✅ [VLO API] Loaded {} actions", actions.len());
    Ok(actions)
}

async fn dev() {
    let root = get_project_root();
    let pages_path = root.join("pages");
    let public_path = root.join("public");
    let public_path2 = public_path.clone();
    let (tx, _) = broadcast::channel::<()>(16);
    let tx_watcher = tx.clone();
    let last_reload = Arc::new(Mutex::new(std::time::Instant::now()));
    std::thread::spawn(move || {
        let _ = watch_files(pages_path, public_path, tx_watcher, last_reload);
    });
    let app = Router::new()
        .route("/", get(home_handler))
        .route(
            "/api",
            get(api_handler_root)
                .post(api_handler_root)
                .put(api_handler_root)
                .patch(api_handler_root)
                .delete(api_handler_root),
        )
        .route(
            "/api/:resource",
            get(api_handler_path)
                .post(api_handler_path)
                .put(api_handler_path)
                .patch(api_handler_path)
                .delete(api_handler_path),
        )
        .route(
            "/api/:resource/:id",
            get(api_handler_id)
                .post(api_handler_id)
                .put(api_handler_id)
                .patch(api_handler_id)
                .delete(api_handler_id),
        )
        .route("/:path", get(page_handler))
        .route("/__vlo_hmr", get(move || hmr_handler(tx)))
        .nest_service("/static", ServeDir::new(public_path2))
        .fallback(not_found_handler);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("Failed to bind port 3000");
    println!("⚡ VLO dev server running at http://localhost:3000");
    println!("📁 Project root: {}", root.display());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");
    println!("\n⚡ Shutting down VLO dev server...");
}

async fn hmr_handler(
    tx: broadcast::Sender<()>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = tx.subscribe();
    let stream = stream! {
        while rx.recv().await.is_ok() {
            yield Ok(Event::default().data("reload"));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn build() {
    println!("⚡ Building production site...");
    let root = get_project_root();
    let pages = root.join("pages");
    let public = root.join("public");
    let dist = root.join("dist");
    if dist.exists() {
        let _ = fs::remove_dir_all(&dist);
    }
    fs::create_dir_all(dist.join("static")).expect("Failed to create dist directory");
    if let Ok(entries) = fs::read_dir(&pages) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("vlo") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_string_lossy().to_string();
            let content = fs::read_to_string(&path).unwrap_or_default();
            let html = wrap_html(&stem, &render_vlo(content), false);
            let output = if stem == "home" || stem == "index" {
                dist.join("index.html")
            } else {
                dist.join(format!("{}.html", stem))
            };
            fs::write(&output, html).expect("Failed to write generated HTML");
            println!("  ├─ Generated: {}", output.display());
        }
    }
    if public.exists() {
        copy_dir_all(&public, &dist.join("static")).expect("Failed to copy static assets");
        println!("  └─ Copied static assets");
    }
    println!("⚡ Build completed successfully!");
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

async fn deploy(provider: &str) {
    let root = get_project_root();
    let dist = root.join("dist");
    if !dist.exists() {
        build();
    }
    let provider = provider.to_lowercase();
    println!("⚡ Deploying /dist to {}...", provider);
    if provider == "railway" {
        let caddy = dist.join("Caddyfile");
        if !caddy.exists() {
            fs::write(&caddy, ":$PORT {\n    root * .\n    file_server\n}\n")
                .expect("Failed to write Caddyfile");
        }
    }
    let args: Vec<&str> = match provider.as_str() {
        "netlify" => vec!["netlify-cli", "deploy", "--dir=dist", "--prod"],
        "vercel" => vec!["vercel", "deploy", "dist", "--prod"],
        "cloudflare" | "pages" => vec!["wrangler", "pages", "deploy", "dist"],
        "railway" => vec!["@railway/cli", "up"],
        _ => {
            eprintln!("❌ Unsupported provider '{}'.", provider);
            return;
        }
    };
    let working_dir = if provider == "railway" { &dist } else { &root };
    let status = if cfg!(target_os = "windows") {
        Command::new("cmd")
            .arg("/C")
            .arg("npx.cmd")
            .arg("-y")
            .args(&args)
            .current_dir(working_dir)
            .status()
    } else {
        Command::new("npx")
            .arg("-y")
            .args(&args)
            .current_dir(working_dir)
            .status()
    };
    match status {
        Ok(s) if s.success() => println!("⚡ Deployment completed successfully!"),
        Ok(s) => eprintln!("❌ Deployment exited with status: {}", s),
        Err(e) => eprintln!("❌ Failed to execute deployment command: {}", e),
    }
}

async fn api_handler_root(
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    payload: Option<Json<Value>>,
) -> impl IntoResponse {
    api_route_handler(None, None, method, query, payload).await
}

async fn api_handler_path(
    AxumPath(resource): AxumPath<String>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    payload: Option<Json<Value>>,
) -> impl IntoResponse {
    api_route_handler(Some(resource), None, method, query, payload).await
}

async fn api_handler_id(
    AxumPath((resource, id)): AxumPath<(String, String)>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    payload: Option<Json<Value>>,
) -> impl IntoResponse {
    api_route_handler(Some(resource), Some(id), method, query, payload).await
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

async fn api_route_handler(
    mut endpoint: Option<String>,
    id: Option<String>,
    method: Method,
    mut query: HashMap<String, String>,
    payload: Option<Json<Value>>,
) -> impl IntoResponse {
    if let Some(Json(body)) = payload {
        if let Value::Object(map) = body {
            for (key, value) in map {
                query.insert(key, value_to_query_string(&value));
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
                "error": "Invalid API resource",
                "resource": resource
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
                    "error": "Unsupported HTTP method",
                    "method": method.as_str(),
                    "allowed_methods": ["GET", "POST", "PUT", "PATCH", "DELETE"]
                })),
            )
                .into_response();
        }
    };
    let explicit_action = endpoint.starts_with("get_")
        || endpoint.starts_with("post_")
        || endpoint.starts_with("put_")
        || endpoint.starts_with("patch_")
        || endpoint.starts_with("delete_");
    let action_name = if explicit_action {
        endpoint.clone()
    } else {
        format!("{}_{}", operation, resource)
    };
    println!(
        "🔎 [VLO API] {} /api/{}{} -> {}",
        method,
        resource,
        id.as_ref()
            .map(|value| format!("/{}", value))
            .unwrap_or_default(),
        action_name
    );
    let actions = match load_api_actions() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("❌ [VLO API] {}", error);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Failed to load API definitions",
                    "details": error,
                    "action": action_name
                })),
            )
                .into_response();
        }
    };
    let mut sql = match actions.get(&action_name) {
        Some(value) => value.clone(),
        None => {
            let mut available = actions.keys().cloned().collect::<Vec<String>>();
            available.sort();
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "success": false,
                    "error": "API operation not found",
                    "operation": operation,
                    "resource": resource,
                    "action": action_name,
                    "available_actions": available
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
            params.insert(key, query_string_to_value(&value));
        }
    }
    if id.is_some() && operation == "get" && !sql.contains("{{id}}") {
        sql = add_id_filter(&sql, &params);
    }
    println!("🗄️ [VLO API] Executing: {}", action_name);
    println!("📝 [VLO API] SQL: {}", sql);
    let mut conn = match get_db_conn() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("❌ [VLO API] Database connection failed: {}", error);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Database connection failed",
                    "details": error.to_string()
                })),
            )
                .into_response();
        }
    };
    match execute_api_sql(&mut conn, &sql, &params) {
        Ok(data) => (StatusCode::OK, Json(data)).into_response(),
        Err(error) => {
            eprintln!("❌ [VLO API] SQL error in {}: {}", action_name, error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": "SQL Execution Error",
                    "details": error.to_string(),
                    "action": action_name,
                    "sql": sql
                })),
            )
                .into_response()
        }
    }
}

fn value_to_sql_value(value: &Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(v) => rusqlite::types::Value::Integer(if *v { 1 } else { 0 }),
        Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                rusqlite::types::Value::Integer(i)
            } else if let Some(u) = v.as_u64() {
                if u <= i64::MAX as u64 {
                    rusqlite::types::Value::Integer(u as i64)
                } else {
                    rusqlite::types::Value::Real(u as f64)
                }
            } else if let Some(f) = v.as_f64() {
                rusqlite::types::Value::Real(f)
            } else {
                rusqlite::types::Value::Text(v.to_string())
            }
        }
        Value::String(v) => rusqlite::types::Value::Text(v.clone()),
        _ => rusqlite::types::Value::Text(value.to_string()),
    }
}

fn prepare_sql(
    sql: &str,
    params: &serde_json::Map<String, Value>,
) -> rusqlite::Result<(String, Vec<rusqlite::types::Value>)> {
    let re = Regex::new(r"\{\{\s*([a-zA-Z0-9_-]+)\s*\}\}")
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let mut values = Vec::new();
    let prepared = re
        .replace_all(sql, |caps: &regex::Captures| {
            let key = &caps[1];
            match params.get(key) {
                Some(value) => {
                    values.push(value_to_sql_value(value));
                    "?".to_string()
                }
                None => format!("{{{{{}}}}}", key),
            }
        })
        .into_owned();
    Ok((prepared, values))
}

fn value_to_sql(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(v) => {
            if *v {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        Value::Number(v) => v.to_string(),
        Value::String(v) => format!("'{}'", v.replace('\'', "''")),
        _ => format!("'{}'", value.to_string().replace('\'', "''")),
    }
}

fn add_id_filter(sql: &str, params: &serde_json::Map<String, Value>) -> String {
    let id = match params.get("id") {
        Some(value) => value_to_sql(value),
        None => return sql.to_string(),
    };
    let upper = sql.to_uppercase();
    if let Some(pos) = upper.find(" ORDER BY ") {
        let before = sql[..pos].trim_end();
        let order = &sql[pos..];
        if before.to_uppercase().contains(" WHERE ") {
            format!("{} AND id = {}{}", before, id, order)
        } else {
            format!("{} WHERE id = {}{}", before, id, order)
        }
    } else {
        let trimmed = sql.trim_end_matches(';').trim_end();
        if trimmed.to_uppercase().contains(" WHERE ") {
            format!("{} AND id = {};", trimmed, id)
        } else {
            format!("{} WHERE id = {};", trimmed, id)
        }
    }
}
/*
fn extract_select(sql: &str) -> Option<String> {
    let mut result = None;
    for statement in sql.split(';') {
        let s = statement.trim();
        if s.to_uppercase().starts_with("SELECT ") {
            result = Some(s.to_string());
        }
    }
    result
}
*/
fn row_to_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut map = serde_json::Map::new();
    for i in 0..row.as_ref().column_count() {
        let name = row.as_ref().column_name(i).unwrap_or("column");
        let value = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(v) => Value::Number(v.into()),
            ValueRef::Real(v) => serde_json::Number::from_f64(v)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            ValueRef::Text(v) => Value::String(String::from_utf8_lossy(v).to_string()),
            ValueRef::Blob(v) => Value::String(format!("blob {}b", v.len())),
        };
        map.insert(name.to_string(), value);
    }
    Ok(Value::Object(map))
}

fn execute_api_sql(
    conn: &mut Connection,
    sql: &str,
    params: &serde_json::Map<String, Value>,
) -> Result<Value> {
    let transaction = conn.transaction()?;
    let mut last_data = None;
    let mut affected_rows = 0usize;
    for statement in sql.split(';') {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        let (prepared_sql, values) = prepare_sql(statement, params)?;
        if prepared_sql.contains("{{") {
            return Err(rusqlite::Error::InvalidParameterName(prepared_sql));
        }
        let upper = prepared_sql.trim_start().to_uppercase();
        if upper.starts_with("SELECT") || upper.starts_with("PRAGMA") || upper.starts_with("WITH") {
            let mut stmt = transaction.prepare(&prepared_sql)?;
            let params_ref: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(rusqlite::params_from_iter(params_ref), row_to_json)?;
            let mut data = Vec::new();
            for row in rows {
                data.push(row?);
            }
            last_data = Some(data);
        } else {
            let params_ref: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            affected_rows +=
                transaction.execute(&prepared_sql, rusqlite::params_from_iter(params_ref))?;
        }
    }
    transaction.commit()?;
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
    if let Ok(v) = value.parse::<i64>() {
        return Value::Number(v.into());
    }
    if let Ok(v) = value.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(v) {
            return Value::Number(n);
        }
    }
    Value::String(value.to_string())
}

async fn home_handler() -> impl IntoResponse {
    render_page("home".to_string(), true).await
}

async fn page_handler(AxumPath(path): AxumPath<String>) -> impl IntoResponse {
    render_page(path, true).await
}

async fn not_found_handler() -> impl IntoResponse {
    render_404(true).await
}

async fn render_404(dev: bool) -> impl IntoResponse {
    tokio::task::spawn_blocking(move || {
        let file = get_project_root().join("pages").join("404.vlo");
        if let Ok(content) = fs::read_to_string(file) {
            (
                StatusCode::NOT_FOUND,
                Html(wrap_html("404 - Page Not Found", &render_vlo(content), dev)),
            )
        } else {
            let fallback = r#"
<div style="
text-align:center;
padding:100px 20px;
font-family:sans-serif;
background:#0a0a0c;
color:#fff;
min-height:80vh">
<h1 style="font-size:7rem;color:#00f5ff">404</h1>
<h2>Page Not Found</h2>
<p style="color:#888">The requested page does not exist.</p>
<a href="/" style="
background:#00f5ff;
color:#000;
padding:12px 28px;
text-decoration:none;
border-radius:8px">
Back to Home
</a>
</div>
"#;
            (
                StatusCode::NOT_FOUND,
                Html(wrap_html("404 - Page Not Found", fallback, dev)),
            )
        }
    })
    .await
    .unwrap_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        Html("Server Error".to_string()),
    ))
}

async fn render_page(path: String, dev: bool) -> impl IntoResponse {
    let p = path.clone();
    match tokio::task::spawn_blocking(move || {
        let file = get_project_root().join("pages").join(format!("{}.vlo", p));
        fs::read_to_string(file).ok().map(|content| {
            (
                StatusCode::OK,
                Html(wrap_html(&p, &render_vlo(content), dev)),
            )
        })
    })
    .await
    {
        Ok(Some(response)) => response.into_response(),
        _ => render_404(dev).await.into_response(),
    }
}

fn render_vlo(mut source: String) -> String {
    source = strip_server_block(&source);
    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, "BaseLayout");
        source = render_components(&source);
        if source == previous {
            break;
        }
    }
    source
}

fn render_tag(source: &str, tag: &str) -> String {
    if let Some((start, end, props, children)) = find_tag(source, tag) {
        return format!(
            "{}{}{}",
            &source[..start],
            render_component_file(tag, &props, &children),
            &source[end..]
        );
    }
    source.to_string()
}

fn render_components(source: &str) -> String {
    let mut output = String::new();
    let mut last = 0;
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (pos, ch) = chars[i];
        if ch == '<' && i + 1 < chars.len() && chars[i + 1].1.is_ascii_uppercase() {
            let mut end = i + 1;
            while end < chars.len() && (chars[end].1.is_ascii_alphanumeric() || chars[end].1 == '_')
            {
                end += 1;
            }
            let tag = &source[chars[i + 1].0..chars[end].0];
            if let Some((_, tag_end, props, children)) = find_tag(&source[pos..], tag) {
                output.push_str(&source[last..pos]);
                output.push_str(&render_component_file(tag, &props, &children));
                last = pos + tag_end;
                while i < chars.len() && chars[i].0 < last {
                    i += 1;
                }
                continue;
            }
        }
        i += 1;
    }
    output.push_str(&source[last..]);
    output
}

fn find_tag(source: &str, name: &str) -> Option<(usize, usize, String, String)> {
    let open = format!("<{}", name);
    let start = source.find(&open)?;
    let mut i = start + open.len();
    let props_start = i;
    let mut quote = None;
    for (offset, ch) in source[i..].char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '>' => {
                i += offset;
                break;
            }
            _ => {}
        }
    }
    if i >= source.len() {
        return None;
    }
    let props = source[props_start..i].to_string();
    let self_close = props.trim_end().ends_with('/');
    i += 1;
    if self_close {
        return Some((start, i, props, String::new()));
    }
    let close = format!("</{}>", name);
    let children_start = i;
    let mut depth = 1;
    while i < source.len() {
        if source[i..].starts_with(&open) {
            depth += 1;
        } else if source[i..].starts_with(&close) {
            depth -= 1;
            if depth == 0 {
                return Some((
                    start,
                    i + close.len(),
                    props,
                    source[children_start..i].to_string(),
                ));
            }
        }
        i += source[i..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
    }
    None
}

fn render_component_file(name: &str, props_str: &str, children: &str) -> String {
    let root = get_project_root();
    for path in [
        root.join("layouts").join(format!("{}.vlo", name)),
        root.join("components").join(format!("{}.vlo", name)),
    ] {
        if let Ok(template) = fs::read_to_string(path) {
            let mut props = parse_props(props_str);
            props.insert("children".to_string(), render_vlo(children.to_string()));
            let regex = Regex::new(r"\{\{\s*([a-zA-Z0-9_-]+)\s*\}\}").unwrap();
            return regex
                .replace_all(&template, |caps: &regex::Captures| {
                    props.get(&caps[1]).cloned().unwrap_or_default()
                })
                .into_owned();
        }
    }
    format!("<!-- {} not found -->", name)
}

fn parse_props(raw: &str) -> HashMap<String, String> {
    let chars: Vec<char> = raw
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .chars()
        .collect();
    let mut map = HashMap::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let mut key = String::new();
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            key.push(chars[i]);
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            let q = chars[i];
            let mut value = String::new();
            if q == '"' || q == '\'' {
                i += 1;
                while i < chars.len() && chars[i] != q {
                    value.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
            } else {
                while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '>' {
                    value.push(chars[i]);
                    i += 1;
                }
            }
            map.insert(key, value);
        } else {
            map.insert(key, "true".to_string());
        }
    }
    map
}

fn wrap_html(title: &str, content: &str, dev: bool) -> String {
    let hmr = if dev {
        r#"<script>
const es=new EventSource("/__vlo_hmr");
es.onmessage=()=>location.reload();
window.addEventListener("beforeunload",()=>es.close());
</script>"#
    } else {
        ""
    };
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<title>{}</title>
<link rel="icon" href="/static/favicon.ico">
<link rel="stylesheet" href="/static/style.css">
{}
</head>
<body>{}</body>
</html>"#,
        title, hmr, content
    )
}

fn watch_files(
    pages: PathBuf,
    public: PathBuf,
    tx: broadcast::Sender<()>,
    last: Arc<Mutex<std::time::Instant>>,
) -> notify::Result<()> {
    let (tx_notify, rx) = channel();
    let mut watcher = RecommendedWatcher::new(tx_notify, Config::default())?;
    if pages.exists() {
        watcher.watch(&pages, RecursiveMode::Recursive)?;
    }
    if public.exists() {
        watcher.watch(&public, RecursiveMode::Recursive)?;
    }
    for result in rx {
        if let Ok(event) = result {
            let relevant = event.paths.iter().any(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("vlo") || p.starts_with(&public)
            });
            if !relevant {
                continue;
            }
            if let Ok(mut timestamp) = last.try_lock() {
                if timestamp.elapsed().as_millis() > 200 {
                    *timestamp = std::time::Instant::now();
                    let _ = tx.send(());
                    println!("⚡ Reload");
                }
            }
        }
    }
    Ok(())
}
