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
use lru::LruCache;
use memchr::memchr;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};
use r2d2::{ManageConnection, Pool};
use rayon::prelude::*;
use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Result as SqliteResult};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;
use std::num::NonZeroUsize;
use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc::channel, Arc, LazyLock, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use tower_http::services::ServeDir;
use tracing::{debug, error, info};

// ============================================================
// VLO CLI
// ============================================================
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

// ============================================================
// PERFORMANCE: SHARED STATE / CACHES
// ============================================================

static PROJECT_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
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
});

static SCHEMA_APPLIED: OnceLock<()> = OnceLock::new();

static STYLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style[^>]*>(.*?)</style>").unwrap());
static ELEMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<([a-zA-Z][a-zA-Z0-9-]*)(\s[^>]*)?>").unwrap());
static CONDITIONAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\{\{#if\s+([a-zA-Z0-9_-]+)\s*\}\}(.*?)\{\{/if\}\}"#).unwrap()
});
static PROP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([a-zA-Z0-9_-]+)\s*\}\}").unwrap());
static CLASS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bclass\s*=\s*("([^"]*)"|'([^']*)')"#).unwrap());
static SLOT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<slot(?:\s+name\s*=\s*["']([^"']+)["'])?\s*>(.*?)</slot>"#).unwrap()
});
static SQL_PARAM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([a-zA-Z0-9_-]+)\s*\}\}").unwrap());

#[derive(Debug)]
struct CompiledTemplate {
    template: Box<str>,
    css: Box<str>,
}

static TEMPLATE_CACHE: LazyLock<RwLock<FxHashMap<PathBuf, Arc<CompiledTemplate>>>> =
    LazyLock::new(|| RwLock::new(FxHashMap::default()));

static API_ACTIONS_CACHE: LazyLock<RwLock<Option<Arc<FxHashMap<String, String>>>>> =
    LazyLock::new(|| RwLock::new(None));

// ============================================================
// DATABASE CONNECTION POOL
// ============================================================

#[derive(Debug, Clone)]
struct SqliteManager {
    path: PathBuf,
}

impl ManageConnection for SqliteManager {
    type Connection = Connection;
    type Error = rusqlite::Error;

    fn connect(&self) -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open(&self.path)?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -20000;
             PRAGMA temp_store = MEMORY;
             PRAGMA mmap_size = 268435456;",
        )?;
        Ok(conn)
    }

    fn is_valid(&self, conn: &mut Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch("SELECT 1;").map(|_| ())
    }

    fn has_broken(&self, _conn: &mut Connection) -> bool {
        false
    }
}

static DB_POOL: LazyLock<Pool<SqliteManager>> = LazyLock::new(|| {
    let root = get_project_root();
    let manager = SqliteManager {
        path: root.join("vlo_app.db"),
    };
    Pool::builder()
        .max_size(16)
        .min_idle(Some(4))
        .connection_timeout(Duration::from_secs(5))
        .build(manager)
        .expect("Failed to create DB pool")
});

fn get_db_conn() -> SqliteResult<r2d2::PooledConnection<SqliteManager>> {
    DB_POOL
        .get()
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn ensure_schema(conn: &Connection) -> SqliteResult<()> {
    if SCHEMA_APPLIED.get().is_some() {
        return Ok(());
    }
    let root = get_project_root();
    let schema = root.join("schema.sql");
    if schema.exists() {
        let sql = fs::read_to_string(&schema)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let sql = sql
            .lines()
            .filter(|line| {
                let trimmed = line.trim().to_uppercase();
                !trimmed.starts_with("CREATE DATABASE") && !trimmed.starts_with("USE ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        conn.execute_batch(&sql)?;
    }
    let _ = SCHEMA_APPLIED.set(());
    Ok(())
}

// ============================================================
// PAGE CACHE (LRU + TTL)
// ============================================================

struct CachedPage {
    html: String,
    rendered_at: Instant,
}

static PAGE_CACHE: LazyLock<Mutex<LruCache<String, CachedPage>>> = LazyLock::new(|| {
    Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap()))
});

const PAGE_TTL: Duration = Duration::from_millis(500);

fn get_cached_page(path: &str) -> Option<String> {
    let mut cache = PAGE_CACHE.lock();
    if let Some(cached) = cache.get(path) {
        if cached.rendered_at.elapsed() < PAGE_TTL {
            return Some(cached.html.clone());
        }
    }
    None
}

fn put_cached_page(path: String, html: String) {
    let mut cache = PAGE_CACHE.lock();
    cache.put(path, CachedPage {
        html,
        rendered_at: Instant::now(),
    });
}

fn invalidate_page_cache() {
    PAGE_CACHE.lock().clear();
}

// ============================================================
// VLO RENDER STATE
// ============================================================
#[derive(Default)]
struct RenderedPage {
    html: String,
    styles: Vec<String>,
}

impl RenderedPage {
    fn add_style(&mut self, name: &str, css: &str) {
        let marker = format!("/* VLO:{} */", name);
        if self.styles.iter().any(|style| style.contains(&marker)) {
            return;
        }
        self.styles.push(format!("{}\n{}", marker, css));
    }
}

// ============================================================
// APPLICATION ENTRY
// ============================================================
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vlo=info")),
        )
        .init();

    match Cli::parse().command {
        Commands::Dev => dev().await,
        Commands::Build => build(),
        Commands::Deploy { provider } => deploy(&provider).await,
    }
}

// ============================================================
// PROJECT
// ============================================================
fn get_project_root() -> PathBuf {
    PROJECT_ROOT.clone()
}

// ============================================================
// VLO SERVER BLOCK
// ============================================================
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

// ============================================================
// VLO API DEFINITION LOADER
// ============================================================
fn load_api_actions() -> std::result::Result<Arc<FxHashMap<String, String>>, String> {
    {
        let cache = API_ACTIONS_CACHE.read();
        if let Some(actions) = cache.as_ref() {
            return Ok(Arc::clone(actions));
        }
    }

    let file = get_project_root().join("pages/api/api.vlo");
    debug!("🔎 [VLO API] Loading: {}", file.display());

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

    let mut actions = FxHashMap::with_capacity_and_hasher(object.len(), Default::default());
    for (name, value) in object {
        if let Some(sql) = value.as_str() {
            actions.insert(name.clone(), sql.to_string());
        }
    }
    let actions = Arc::new(actions);
    debug!("✅ [VLO API] Loaded {} actions", actions.len());

    let mut cache = API_ACTIONS_CACHE.write();
    if let Some(cached) = cache.as_ref() {
        return Ok(Arc::clone(cached));
    }
    *cache = Some(Arc::clone(&actions));
    Ok(actions)
}

// ============================================================
// DEVELOPMENT SERVER
// ============================================================
async fn dev() {
    let root = get_project_root();
    let pages_path = root.join("pages");
    let public_path = root.join("public");
    let public_path_service = public_path.clone();

    let (tx, _) = broadcast::channel::<()>(16);
    let tx_watcher = tx.clone();
    let last_reload = Arc::new(Mutex::new(Instant::now()));

    std::thread::spawn(move || {
        let _ = watch_files(pages_path, public_path, tx_watcher, last_reload);
    });

    let app = Router::new()
        .route("/", get(home_handler))
        .route("/:path", get(page_handler))
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
        .route("/__vlo_hmr", get(move || hmr_handler(tx)))
        .nest_service("/static", ServeDir::new(public_path_service))
        .fallback(not_found_handler);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("Failed to bind port 3000");
    info!("⚡ VLO dev server running at http://localhost:3000");
    info!("📁 Project root: {}", root.display());

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");
    info!("\n⚡ Shutting down VLO dev server...");
}

// ============================================================
// HOT MODULE RELOAD
// ============================================================
async fn hmr_handler(tx: broadcast::Sender<()>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = tx.subscribe();
    let stream = stream! {
        while rx.recv().await.is_ok() {
            yield Ok(Event::default().data("reload"));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn watch_files(
    pages: PathBuf,
    public: PathBuf,
    tx: broadcast::Sender<()>,
    last: Arc<Mutex<Instant>>,
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
        let Ok(event) = result else {
            continue;
        };
        let relevant = event.paths.iter().any(|path| {
            path.extension().and_then(|e| e.to_str()) == Some("vlo") || path.starts_with(&public)
        });
        if !relevant {
            continue;
        }
        {
            let mut timestamp = last.lock();
            if timestamp.elapsed().as_millis() > 200 {
                *timestamp = Instant::now();
                TEMPLATE_CACHE.write().clear();
                API_ACTIONS_CACHE.write().take();
                invalidate_page_cache();
                let _ = tx.send(());
                info!("⚡ Reload");
            }
        }
    }
    Ok(())
}

// ============================================================
// PRODUCTION BUILD (parallelized with rayon)
// ============================================================
fn build() {
    info!("⚡ Building production site...");
    let root = get_project_root();
    let pages = root.join("pages");
    let public = root.join("public");
    let dist = root.join("dist");

    if dist.exists() {
        let _ = fs::remove_dir_all(&dist);
    }
    fs::create_dir_all(dist.join("static")).expect("Failed to create dist directory");

    let page_entries: Vec<_> = if let Ok(entries) = fs::read_dir(&pages) {
        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("vlo") {
                    return None;
                }
                let stem = path.file_stem()?.to_string_lossy().to_string();
                let content = fs::read_to_string(&path).ok()?;
                Some((stem, content, path))
            })
            .collect()
    } else {
        vec![]
    };

    let results: Vec<_> = page_entries
        .par_iter()
        .map(|(stem, content, _path)| {
            let rendered = render_vlo(content.clone());
            let html = wrap_html(stem, &rendered, false);
            let output = if stem == "home" || stem == "index" {
                dist.join("index.html")
            } else {
                dist.join(format!("{}.html", stem))
            };
            (output, html)
        })
        .collect();

    for (output, html) in results {
        fs::write(&output, html).expect("Failed to write generated HTML");
        info!("  ├─ Generated: {}", output.display());
    }

    if public.exists() {
        copy_dir_all(&public, &dist.join("static")).expect("Failed to copy static assets");
        info!("  └─ Copied static assets");
    }
    info!("⚡ Build completed successfully!");
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

// ============================================================
// DEPLOYMENT
// ============================================================
async fn deploy(provider: &str) {
    let root = get_project_root();
    let dist = root.join("dist");
    if !dist.exists() {
        build();
    }

    let provider = provider.to_lowercase();
    info!("⚡ Deploying /dist to {}...", provider);

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
            error!("❌ Unsupported provider '{}'.", provider);
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
        Ok(status) if status.success() => {
            info!("⚡ Deployment completed successfully!");
        }
        Ok(status) => {
            error!("❌ Deployment exited with status: {}", status);
        }
        Err(error) => {
            error!("❌ Failed to execute deployment command: {}", error);
        }
    }
}

// ============================================================
// API ROUTES
// ============================================================
async fn api_handler_root(
    method: Method,
    Query(query): Query<FxHashMap<String, String>>,
    payload: Option<Json<Value>>,
) -> impl IntoResponse {
    api_route_handler(None, None, method, query, payload).await
}

async fn api_handler_path(
    AxumPath(resource): AxumPath<String>,
    method: Method,
    Query(query): Query<FxHashMap<String, String>>,
    payload: Option<Json<Value>>,
) -> impl IntoResponse {
    api_route_handler(Some(resource), None, method, query, payload).await
}

async fn api_handler_id(
    AxumPath((resource, id)): AxumPath<(String, String)>,
    method: Method,
    Query(query): Query<FxHashMap<String, String>>,
    payload: Option<Json<Value>>,
) -> impl IntoResponse {
    api_route_handler(Some(resource), Some(id), method, query, payload).await
}

// ============================================================
// API HELPERS
// ============================================================
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

// ============================================================
// API REQUEST HANDLER
// ============================================================
async fn api_route_handler(
    mut endpoint: Option<String>,
    id: Option<String>,
    method: Method,
    mut query: FxHashMap<String, String>,
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
                        "actions": actions.as_ref()
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

    let explicit_action = ["get_", "post_", "put_", "patch_", "delete_"]
        .iter()
        .any(|prefix| endpoint.starts_with(prefix));
    let action_name = if explicit_action {
        endpoint.clone()
    } else {
        format!("{}_{}", operation, resource)
    };

    debug!(
        "🔎 [VLO API] {} /api/{}{} -> {}",
        method,
        resource,
        id.as_ref().map(|v| format!("/{}", v)).unwrap_or_default(),
        action_name
    );

    let actions = match load_api_actions() {
        Ok(value) => value,
        Err(error) => {
            error!("❌ [VLO API] {}", error);
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

    debug!("🗄️ [VLO API] Executing: {}", action_name);
    debug!("📝 [VLO API] SQL: {}", sql);

    let conn = match get_db_conn() {
        Ok(value) => value,
        Err(error) => {
            error!("❌ [VLO API] Database connection failed: {}", error);
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

    if let Err(error) = ensure_schema(&conn) {
        error!("❌ [VLO API] Schema application failed: {}", error);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "success": false,
                "error": "Schema application failed",
                "details": error.to_string()
            })),
        )
            .into_response();
    }

    match execute_api_sql(conn, &sql, &params) {
        Ok(data) => (StatusCode::OK, Json(data)).into_response(),
        Err(error) => {
            error!("❌ [VLO API] SQL error in {}: {}", action_name, error);
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

// ============================================================
// API / SQL VALUES
// ============================================================
fn value_to_sql_value(value: &Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(value) => rusqlite::types::Value::Integer(if *value { 1 } else { 0 }),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                rusqlite::types::Value::Integer(integer)
            } else if let Some(unsigned) = value.as_u64() {
                if unsigned <= i64::MAX as u64 {
                    rusqlite::types::Value::Integer(unsigned as i64)
                } else {
                    rusqlite::types::Value::Real(unsigned as f64)
                }
            } else if let Some(float) = value.as_f64() {
                rusqlite::types::Value::Real(float)
            } else {
                rusqlite::types::Value::Text(value.to_string())
            }
        }
        Value::String(value) => rusqlite::types::Value::Text(value.clone()),
        _ => rusqlite::types::Value::Text(value.to_string()),
    }
}

fn prepare_sql(
    sql: &str,
    params: &serde_json::Map<String, Value>,
) -> rusqlite::Result<(String, Vec<rusqlite::types::Value>)> {
    if !sql.contains("{{") {
        return Ok((sql.to_string(), Vec::new()));
    }
    let mut values = Vec::new();
    let prepared = SQL_PARAM_RE
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
        Value::Bool(value) => {
            if *value {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        Value::Number(value) => value.to_string(),
        Value::String(value) => format!("'{}'", value.replace('\'', "''")),
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

// ============================================================
// API / DATABASE ROWS
// ============================================================
fn row_to_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut map = serde_json::Map::new();
    for index in 0..row.as_ref().column_count() {
        let name = row.as_ref().column_name(index).unwrap_or("column");
        let value = match row.get_ref(index)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(value) => Value::Number(value.into()),
            ValueRef::Real(value) => serde_json::Number::from_f64(value)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).to_string()),
            ValueRef::Blob(value) => Value::String(format!("blob {}b", value.len())),
        };
        map.insert(name.to_string(), value);
    }
    Ok(Value::Object(map))
}

// ============================================================
// API / SQL EXECUTION (with read-only fast path)
// ============================================================
#[inline]
fn is_query_sql(sql: &str) -> bool {
    let sql = sql.trim_start();
    sql.get(..6)
        .map(|v| v.eq_ignore_ascii_case("SELECT"))
        .unwrap_or(false)
        || sql.get(..6)
            .map(|v| v.eq_ignore_ascii_case("PRAGMA"))
            .unwrap_or(false)
        || sql.get(..4)
            .map(|v| v.eq_ignore_ascii_case("WITH"))
            .unwrap_or(false)
}

fn execute_api_sql(
    mut conn: r2d2::PooledConnection<SqliteManager>,
    sql: &str,
    params: &serde_json::Map<String, Value>,
) -> SqliteResult<Value> {
    let statements: Vec<&str> = sql
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let is_read_only = statements.iter().all(|s| is_query_sql(s));

    if is_read_only && statements.len() == 1 {
        let stmt_sql = statements[0];
        let (prepared_sql, values) = prepare_sql(stmt_sql, params)?;
        if prepared_sql.contains("{{") {
            return Err(rusqlite::Error::InvalidParameterName(prepared_sql));
        }
        let params_ref: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&prepared_sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params_ref), row_to_json)?;
        let mut data = Vec::new();
        for row in rows {
            data.push(row?);
        }
        return Ok(serde_json::json!({
            "data": data,
            "affected_rows": 0
        }));
    }

    let transaction = conn.transaction()?;
    let mut last_data = None;
    let mut affected_rows = 0usize;

    for statement in &statements {
        let (prepared_sql, values) = prepare_sql(statement, params)?;
        if prepared_sql.contains("{{") {
            return Err(rusqlite::Error::InvalidParameterName(prepared_sql));
        }
        let params_ref: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();

        if is_query_sql(&prepared_sql) {
            let mut stmt = transaction.prepare(&prepared_sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params_ref), row_to_json)?;
            let mut data = Vec::new();
            for row in rows {
                data.push(row?);
            }
            last_data = Some(data);
        } else {
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
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
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

// ============================================================
// PAGE ROUTES
// ============================================================
async fn home_handler() -> impl IntoResponse {
    render_page("home".to_string(), true).await
}

async fn page_handler(AxumPath(path): AxumPath<String>) -> impl IntoResponse {
    render_page(path, true).await
}

async fn not_found_handler() -> impl IntoResponse {
    render_404(true).await
}

// ============================================================
// PAGE RENDERING (with page cache)
// ============================================================
async fn render_page(path: String, dev: bool) -> impl IntoResponse {
    if dev {
        if let Some(cached) = get_cached_page(&path) {
            return (StatusCode::OK, Html(cached)).into_response();
        }
    }

    let page_path = path.clone();
    match tokio::task::spawn_blocking(move || {
        let file = get_project_root().join("pages").join(format!("{}.vlo", page_path));
        fs::read_to_string(file).ok().map(|content| {
            let rendered = render_vlo(content);
            let html = wrap_html(&page_path, &rendered, dev);
            (page_path, html)
        })
    })
    .await
    {
        Ok(Some((page_path, html))) => {
            if dev {
                put_cached_page(page_path, html.clone());
            }
            (StatusCode::OK, Html(html)).into_response()
        }
        _ => render_404(dev).await.into_response(),
    }
}

async fn render_404(dev: bool) -> impl IntoResponse {
    tokio::task::spawn_blocking(move || {
        let file = get_project_root().join("pages").join("404.vlo");
        if let Ok(content) = fs::read_to_string(file) {
            let rendered = render_vlo(content);
            (
                StatusCode::NOT_FOUND,
                Html(wrap_html("404 - Page Not Found", &rendered, dev)),
            )
        } else {
            let fallback = r#"
<div style="text-align:center;padding:100px 20px;font-family:sans-serif;background:#0a0a0c;color:#fff;min-height:80vh">
    <h1 style="font-size:7rem;color:#00f5ff">404</h1>
    <h2>Page Not Found</h2>
    <p style="color:#888">The requested page does not exist.</p>
    <a href="/" style="background:#00f5ff;color:#000;padding:12px 28px;text-decoration:none;border-radius:8px">Back to Home</a>
</div>
"#;
            let rendered = RenderedPage {
                html: fallback.to_string(),
                ..Default::default()
            };
            (
                StatusCode::NOT_FOUND,
                Html(wrap_html("404 - Page Not Found", &rendered, dev)),
            )
        }
    })
    .await
    .unwrap_or((StatusCode::INTERNAL_SERVER_ERROR, Html("Server Error".to_string())))
}

// ============================================================
// VLO PAGE PIPELINE
// ============================================================
fn render_vlo(source: String) -> RenderedPage {
    let mut context = RenderedPage::default();
    let source = strip_server_block(&source);
    let source = render_tag(&source, "BaseLayout", &mut context);
    let source = render_components(&source, &mut context);
    context.html = source;
    context
}

// ============================================================
// VLO TAG RENDERING
// ============================================================
fn render_tag(source: &str, tag: &str, context: &mut RenderedPage) -> String {
    if let Some((start, end, props, children)) = find_tag(source, tag) {
        return format!(
            "{}{}{}",
            &source[..start],
            render_component_file(tag, &props, &children, context),
            &source[end..],
        );
    }
    source.to_string()
}

// ============================================================
// COMPONENT RENDERING (optimized: no Vec<(usize, char)> alloc)
// ============================================================
fn render_components(source: &str, context: &mut RenderedPage) -> String {
    let mut output = String::with_capacity(source.len() + source.len() / 8);
    let mut last = 0;
    let bytes = source.as_bytes();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let next_lt = match memchr(b'<', &bytes[cursor..]) {
            Some(offset) => cursor + offset,
            None => break,
        };

        let after_lt = next_lt + 1;
        if after_lt < bytes.len() && bytes[after_lt].is_ascii_uppercase() {
            let mut tag_end = after_lt;
            while tag_end < bytes.len()
                && (bytes[tag_end].is_ascii_alphanumeric() || bytes[tag_end] == b'_')
            {
                tag_end += 1;
            }
            if tag_end > after_lt {
                let tag = &source[after_lt..tag_end];
                if let Some((_, full_end, props, children)) = find_tag(&source[next_lt..], tag) {
                    output.push_str(&source[last..next_lt]);
                    output.push_str(&render_component_file(tag, &props, &children, context));
                    last = next_lt + full_end;
                    cursor = next_lt + full_end;
                    continue;
                }
            }
        }
        cursor = next_lt + 1;
    }

    output.push_str(&source[last..]);
    output
}

// ============================================================
// COMPONENT LOOKUP
// ============================================================
fn component_path(name: &str) -> Option<PathBuf> {
    let root = get_project_root();
    let layout = root.join("layouts").join(format!("{}.vlo", name));
    if layout.exists() {
        return Some(layout);
    }
    let component = root.join("components").join(format!("{}.vlo", name));
    if component.exists() {
        return Some(component);
    }
    None
}

fn read_component_template(path: &Path) -> Option<Arc<CompiledTemplate>> {
    {
        let cache = TEMPLATE_CACHE.read();
        if let Some(cached) = cache.get(path) {
            return Some(Arc::clone(cached));
        }
    }

    let source = fs::read_to_string(path).ok()?;

    let mut css = String::with_capacity(source.len() / 4);
    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) {
            css.push_str(style.as_str());
            css.push('\n');
        }
    }
    let template = STYLE_RE.replace_all(&source, "").into_owned();

    let compiled = Arc::new(CompiledTemplate {
        template: template.into_boxed_str(),
        css: css.into_boxed_str(),
    });

    let mut cache = TEMPLATE_CACHE.write();
    cache.insert(path.to_path_buf(), Arc::clone(&compiled));
    Some(compiled)
}

// ============================================================
// COMPONENT TAG PARSER
// ============================================================
fn find_tag(source: &str, name: &str) -> Option<(usize, usize, String, String)> {
    let open = format!("<{}", name);
    let start = source.find(&open)?;

    let mut index = start + open.len();
    let next = source[index..].chars().next()?;
    if !(next.is_whitespace() || next == '/' || next == '>') {
        return None;
    }

    let props_start = index;
    let mut quote = None;
    let mut open_end = None;
    for (offset, character) in source[index..].char_indices() {
        match quote {
            Some(current) if character == current => {
                quote = None;
            }
            None if character == '"' || character == '\'' => {
                quote = Some(character);
            }
            None if character == '>' => {
                open_end = Some(index + offset);
                break;
            }
            _ => {}
        }
    }

    let open_end = open_end?;
    let props = source[props_start..open_end].to_string();
    let self_closing = props.trim_end().ends_with('/');
    index = open_end + 1;

    if self_closing {
        return Some((start, index, props, String::new()));
    }

    let close = format!("</{}>", name);
    let children_start = index;
    let mut depth = 1;
    while index < source.len() {
        let remaining = &source[index..];
        if remaining.starts_with(&open) {
            let after = index + open.len();
            let valid = source[after..]
                .chars()
                .next()
                .map(|c| c.is_whitespace() || c == '/' || c == '>')
                .unwrap_or(false);
            if valid {
                depth += 1;
            }
        } else if remaining.starts_with(&close) {
            depth -= 1;
            if depth == 0 {
                return Some((
                    start,
                    index + close.len(),
                    props,
                    source[children_start..index].to_string(),
                ));
            }
        }
        index += remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
    }
    None
}

// ============================================================
// COMPONENT FILE RENDERER
// ============================================================
fn render_component_file(
    name: &str,
    props_str: &str,
    children: &str,
    context: &mut RenderedPage,
) -> String {
    let path = match component_path(name) {
        Some(path) => path,
        None => {
            return format!("<!-- {} not found -->", name);
        }
    };

    let compiled = match read_component_template(&path) {
        Some(template) => template,
        None => {
            return format!("<!-- {} could not be read -->", name);
        }
    };

    if !compiled.css.trim().is_empty() {
        context.add_style(name, compiled.css.trim());
    }

    let template = &compiled.template;
    let mut props = parse_props(props_str);
    let (raw_named_slots, raw_default_slot) = parse_slot_content(children);

    debug!(
        "🧩 [VLO SLOT] Component <{}> received named slots: {:?}",
        name,
        raw_named_slots.keys().collect::<Vec<_>>()
    );
    debug!(
        "🎯 [VLO SLOT] Component <{}> default slot length={}",
        name,
        raw_default_slot.len()
    );

    let mut named_slots = FxHashMap::default();
    for (slot_name, slot_content) in raw_named_slots {
        let rendered = render_nested_vlo_content(&slot_content, context);
        debug!(
            "🧩 [VLO SLOT] Rendered named slot '{}' length={}",
            slot_name,
            rendered.len()
        );
        named_slots.insert(slot_name, rendered);
    }

    let default_slot = render_nested_vlo_content(&raw_default_slot, context);
    debug!("🎯 [VLO SLOT] Rendered default slot length={}", default_slot.len());

    props.insert("children".to_string(), default_slot.clone());

    let rendered = render_component_template(template, &props);
    let rendered = render_slots(&rendered, &named_slots, &default_slot);
    let attributes = build_component_attributes(template, &props);

    if attributes.is_empty() {
        return rendered;
    }

    if let Some(captures) = ELEMENT_RE.captures(&rendered) {
        let full_match = captures.get(0).unwrap();
        let tag_name = captures.get(1).unwrap().as_str();
        let existing_attributes = captures.get(2).map(|v| v.as_str()).unwrap_or("");
        let replacement = if existing_attributes.trim().is_empty() {
            format!("<{} {}>", tag_name, attributes)
        } else {
            format!("<{}{} {}>", tag_name, existing_attributes, attributes)
        };
        return format!(
            "{}{}{}",
            &rendered[..full_match.start()],
            replacement,
            &rendered[full_match.end()..],
        );
    }
    rendered
}

// ============================================================
// NESTED VLO CONTENT
// ============================================================
fn render_nested_vlo_content(content: &str, context: &mut RenderedPage) -> String {
    if content.trim().is_empty() {
        return String::new();
    }
    let mut source = strip_server_block(content);
    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, "BaseLayout", context);
        source = render_components(&source, context);
        if source == previous {
            break;
        }
    }
    source
}

// ============================================================
// COMPONENT TEMPLATE
// ============================================================
fn render_component_template(template: &str, props: &FxHashMap<String, String>) -> String {
    let button_default = template.to_ascii_lowercase().contains("<button");

    let mut rendered = CONDITIONAL_RE
        .replace_all(template, |captures: &regex::Captures| {
            let key = captures[1].trim();
            let value = props
                .get(key)
                .map(|v| v.trim())
                .or_else(|| {
                    if button_default && key == "type" && !props.contains_key("type") {
                        Some("button")
                    } else {
                        None
                    }
                })
                .unwrap_or("");
            if value.is_empty() || value.eq_ignore_ascii_case("false") {
                String::new()
            } else {
                captures[2].to_string()
            }
        })
        .into_owned();

    rendered = PROP_RE
        .replace_all(&rendered, |captures: &regex::Captures| {
            let key = captures[1].trim();
            if let Some(value) = props.get(key) {
                value.clone()
            } else if button_default && key == "type" {
                "button".to_string()
            } else {
                String::new()
            }
        })
        .into_owned();

    normalize_class_attributes(&rendered)
}

fn normalize_class_attributes(html: &str) -> String {
    CLASS_RE
        .replace_all(html, |captures: &regex::Captures| {
            let value = captures
                .get(2)
                .or_else(|| captures.get(3))
                .map(|v| v.as_str())
                .unwrap_or("");
            let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
            let quote = if captures
                .get(1)
                .map(|v| v.as_str())
                .unwrap_or("")
                .starts_with('"')
            {
                '"'
            } else {
                '\''
            };
            format!("class={quote}{normalized}{quote}")
        })
        .into_owned()
}

// ============================================================
// COMPONENT ATTRIBUTE FORWARDING
// ============================================================
fn build_component_attributes(template: &str, props: &FxHashMap<String, String>) -> String {
    let mut used = FxHashSet::default();
    for captures in PROP_RE.captures_iter(template) {
        used.insert(captures[1].to_string());
    }

    let mut attributes = Vec::with_capacity(8);
    let button_default = template.to_ascii_lowercase().contains("<button");

    if button_default && !props.contains_key("type") && !used.contains("type") {
        attributes.push("type=\"button\"".to_string());
    }

    for (key, value) in props {
        if key == "children" || key == "attributes" {
            continue;
        }
        if used.contains(key) {
            continue;
        }
        if is_boolean_attribute(key) {
            if value.eq_ignore_ascii_case("true") || value == key {
                attributes.push(key.clone());
            }
            continue;
        }
        if value.trim().is_empty() {
            continue;
        }
        attributes.push(format!("{}=\"{}\"", key, escape_html_attribute(value)));
    }
    attributes.sort();
    attributes.join(" ")
}

fn is_boolean_attribute(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "allowfullscreen"
            | "async"
            | "autofocus"
            | "autoplay"
            | "checked"
            | "controls"
            | "default"
            | "defer"
            | "disabled"
            | "formnovalidate"
            | "hidden"
            | "inert"
            | "ismap"
            | "itemscope"
            | "loop"
            | "multiple"
            | "muted"
            | "nomodule"
            | "novalidate"
            | "open"
            | "playsinline"
            | "readonly"
            | "required"
            | "reversed"
            | "selected"
    )
}

#[inline]
fn escape_html_attribute(value: &str) -> String {
    v_htmlescape::escape(value).to_string()
}

// ============================================================
// COMPONENT PROPS
// ============================================================
fn parse_props(raw: &str) -> FxHashMap<String, String> {
    // Normalize smart (curly) quotes to straight quotes before parsing.
    // Using Unicode escapes to avoid any source-encoding issues.
    let normalized: String = raw
        .replace('\u{201C}', "\"") // LEFT DOUBLE QUOTATION MARK  "
        .replace('\u{201D}', "\"") // RIGHT DOUBLE QUOTATION MARK "
        .replace('\u{2018}', "'")  // LEFT SINGLE QUOTATION MARK  '
        .replace('\u{2019}', "'")  // RIGHT SINGLE QUOTATION MARK '
        .chars()
        .collect();

    let chars: Vec<char> = normalized.chars().collect();
    let mut map = FxHashMap::default();
    let mut index = 0;

    while index < chars.len() {
        // Skip whitespace
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        if index >= chars.len() || chars[index] == '/' {
            break;
        }

        // Read key
        let mut key = String::new();
        while index < chars.len()
            && chars[index] != '='
            && !chars[index].is_whitespace()
            && chars[index] != '/'
        {
            key.push(chars[index]);
            index += 1;
        }
        if key.is_empty() {
            index += 1;
            continue;
        }

        // Skip whitespace
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }

        // Check for `=`
        if index < chars.len() && chars[index] == '=' {
            index += 1;
            // Skip whitespace after `=`
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            if index >= chars.len() {
                map.insert(key, "true".to_string());
                break;
            }

            let quote = chars[index];
            let mut value = String::new();

            if quote == '"' || quote == '\'' {
                // Quoted value: read until matching quote
                index += 1;
                while index < chars.len() && chars[index] != quote {
                    value.push(chars[index]);
                    index += 1;
                }
                if index < chars.len() {
                    index += 1; // consume closing quote
                }
            } else {
                // Unquoted value: read until whitespace or `/`
                while index < chars.len()
                    && !chars[index].is_whitespace()
                    && chars[index] != '/'
                {
                    value.push(chars[index]);
                    index += 1;
                }
            }
            map.insert(key, value);
        } else {
            // Boolean shorthand: `<Button disabled />` → disabled="true"
            map.insert(key, "true".to_string());
        }
    }
    map
}

// ============================================================
// SLOT RENDERING
// ============================================================
fn render_slots(
    template: &str,
    named_slots: &FxHashMap<String, String>,
    default_slot: &str,
) -> String {
    debug!(
        "🎯 [VLO SLOT] Rendering slots: {:?}, default_len={}",
        named_slots.keys().collect::<Vec<_>>(),
        default_slot.len()
    );
    SLOT_RE
        .replace_all(template, |captures: &regex::Captures| {
            let name = captures.get(1).map(|v| v.as_str().trim()).unwrap_or("");
            let fallback = captures.get(2).map(|v| v.as_str()).unwrap_or("");
            debug!("🔎 [VLO SLOT] Template slot requested: '{}'", name);
            if name.is_empty() {
                debug!(
                    "➡️ [VLO SLOT] Using default slot, content length={}",
                    default_slot.len()
                );
                if default_slot.trim().is_empty() {
                    fallback.to_string()
                } else {
                    default_slot.to_string()
                }
            } else if let Some(content) = named_slots.get(name) {
                debug!(
                    "✅ [VLO SLOT] Matched '{}' with content length={}",
                    name,
                    content.len()
                );
                content.clone()
            } else {
                debug!("⚠️ [VLO SLOT] No content for '{}', using fallback", name);
                fallback.to_string()
            }
        })
        .into_owned()
}

// ============================================================
// SLOT CONTENT PARSER
// ============================================================
fn parse_slot_content(children: &str) -> (FxHashMap<String, String>, String) {
    let mut named_slots = FxHashMap::default();
    let mut default_content = String::with_capacity(children.len());
    let mut cursor = 0;
    let mut default_start = 0;
    let bytes = children.as_bytes();

    while cursor < bytes.len() {
        let remaining = &bytes[cursor..];
        let open_start = match memchr(b'<', remaining) {
            Some(offset) => cursor + offset,
            None => break,
        };

        let tag_info = match parse_element_at(children, open_start) {
            Some(info) => info,
            None => {
                cursor = open_start + 1;
                continue;
            }
        };

        let (tag_name, opening_end, element_end, props, content) = tag_info;
        if let Some(slot_name) = get_slot_name(&props) {
            if open_start > default_start {
                default_content.push_str(&children[default_start..open_start]);
            }
            debug!("🧩 [VLO SLOT] Found named slot '{}' in <{}>", slot_name, tag_name);
            named_slots
                .entry(slot_name)
                .or_insert_with(String::new)
                .push_str(content);
            cursor = element_end;
            default_start = cursor;
            let _ = opening_end;
            continue;
        }
        cursor = element_end;
    }

    if default_start < children.len() {
        default_content.push_str(&children[default_start..]);
    }

    debug!(
        "🧩 [VLO SLOT] Named slots: {:?}",
        named_slots.keys().collect::<Vec<_>>()
    );
    (named_slots, default_content)
}

fn get_slot_name(props: &str) -> Option<String> {
    let props = parse_props(props);
    props.get("slot").and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn parse_element_at(source: &str, start: usize) -> Option<(String, usize, usize, String, &str)> {
    if !source[start..].starts_with('<') {
        return None;
    }
    let mut cursor = start + 1;
    if cursor >= source.len() {
        return None;
    }
    let first = source[cursor..].chars().next()?;
    if first == '/' || first == '!' || first == '?' {
        return None;
    }

    let tag_start = cursor;
    while cursor < source.len() {
        let ch = source[cursor..].chars().next()?;
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == ':' {
            cursor += ch.len_utf8();
        } else {
            break;
        }
    }
    if cursor == tag_start {
        return None;
    }

    let tag_name = source[tag_start..cursor].to_string();
    let opening_end = find_tag_opening_end(source, cursor)?;
    let props = source[cursor..opening_end].to_string();
    let self_closing = props.trim_end().ends_with('/');

    if self_closing {
        return Some((tag_name, opening_end + 1, opening_end + 1, props, ""));
    }

    let content_start = opening_end + 1;
    let element_end = find_matching_tag_end(source, content_start, &tag_name)?;
    let close_start = element_end.checked_sub(format!("</{}>", tag_name).len())?;
    if close_start < content_start {
        return None;
    }
    let content = &source[content_start..close_start];
    Some((tag_name, opening_end + 1, element_end, props, content))
}

fn find_tag_opening_end(source: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    let mut cursor = start;
    while cursor < source.len() {
        let ch = source[cursor..].chars().next()?;
        match quote {
            Some(active) => {
                if ch == active {
                    quote = None;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if ch == '>' {
                    return Some(cursor);
                }
            }
        }
        cursor += ch.len_utf8();
    }
    None
}

fn find_matching_tag_end(source: &str, start: usize, tag_name: &str) -> Option<usize> {
    let opening = format!("<{}", tag_name);
    let closing = format!("</{}>", tag_name);
    let mut depth = 1;
    let mut cursor = start;

    while cursor < source.len() {
        let remaining = &source[cursor..];
        if remaining.starts_with(&closing) {
            depth -= 1;
            if depth == 0 {
                return Some(cursor + closing.len());
            }
            cursor += closing.len();
            continue;
        }
        if remaining.starts_with(&opening) {
            let after_name = cursor + opening.len();
            let valid = source[after_name..]
                .chars()
                .next()
                .map(|ch| ch.is_whitespace() || ch == '>' || ch == '/')
                .unwrap_or(false);
            if valid {
                let opening_end = find_tag_opening_end(source, after_name)?;
                let props = source[after_name..opening_end].trim();
                if !props.ends_with('/') {
                    depth += 1;
                }
                cursor = opening_end + 1;
                continue;
            }
        }
        cursor += remaining.chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
    }
    None
}

// ============================================================
// HTML DOCUMENT
// ============================================================
fn wrap_html(title: &str, rendered: &RenderedPage, dev: bool) -> String {
    let hmr = if dev {
        r#"<script>
const es=new EventSource("/__vlo_hmr");
es.onmessage=()=>location.reload();
window.addEventListener("beforeunload",()=>es.close());
</script>"#
    } else {
        ""
    };

    let component_styles = if rendered.styles.is_empty() {
        String::new()
    } else {
        format!("<style>\n{}\n</style>", rendered.styles.join("\n"))
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<title>{}</title>
<link rel="icon" href="/static/favicon.ico">
<link rel="stylesheet" href="/static/style.css">
{}
{}
</head>
<body>{}</body>
</html>"#,
        title, component_styles, hmr, rendered.html,
    )
}