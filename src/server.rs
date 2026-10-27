// ─────────────────────────────────────────────────────────────
// server.rs — VLO web server (dev + production)
// Optimized: single-pass directives, Cow-based data resolution,
// zero-copy page lookup, cache invalidation, correct SSE in prod.
// No new dependencies. All features preserved.
// ─────────────────────────────────────────────────────────────
use crate::{
    api::{api_handler_id, api_handler_path, api_handler_root},
    auth,
    features::{is_enabled, Feature},
    files_api::serve_file,
    router::{hmr_handler, home_handler, page_handler, watch_files, page_handler_any},
    state::{self, get_project_root},
    template::escape_html_attribute,
    component::parse_props_v7,
    auth::LAYOUT_TAG_RE,
};

use axum::{
    extract::{DefaultBodyLimit, Request},
    http::{
        header::{CACHE_CONTROL, HeaderName, HeaderValue},
        StatusCode, Uri,
    },
    response::Redirect,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Subcommand;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, LazyLock, Mutex, OnceLock, RwLock},
    time::Instant,
};
use tokio::sync::broadcast;
use tower_http::{
    compression::{predicate::NotForContentType, CompressionLayer},
    services::ServeDir,
};

// ─────────────────────────────────────────────────────────────
// Build cache (swappable — `vlo build` invalidates it)
// ─────────────────────────────────────────────────────────────
struct BuildCache {
    manifest: serde_json::Value,
    manifest_trusted: bool,
    pages: HashMap<String, Arc<str>>,
    initialized: bool,
}

impl BuildCache {
    fn empty() -> Self {
        Self {
            manifest: serde_json::Value::Object(serde_json::Map::new()),
            manifest_trusted: true,
            pages: HashMap::new(),
            initialized: false,
        }
    }

    fn load(build_dir: &Path) -> Self {
        let manifest_path = build_dir.join("routes.json");
        let (manifest, manifest_trusted) = match fs::read_to_string(&manifest_path) {
            Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(value) if value.is_object() => (value, true),
                _ => (serde_json::Value::Object(serde_json::Map::new()), false),
            },
            Err(_) => (serde_json::Value::Object(serde_json::Map::new()), false),
        };

        let mut pages = HashMap::new();
        walk_html_files(build_dir, build_dir, &mut pages);

        Self { manifest, manifest_trusted, pages, initialized: true }
    }
}

/// Recursively scan `dir` for `.html` files and insert them into `pages`.
///
/// Path mapping rules:
///   index.html           → /
///   admin/index.html     → /admin
///   admin/users.html     → /admin/users
///   admin/users/edit.html→ /admin/users/edit
///
/// Non-page directories are skipped:
///   static/              → handled by ServeDir, not by page lookup
fn walk_html_files(dir: &Path, build_dir: &Path, pages: &mut HashMap<String, Arc<str>>) {
    let Ok(entries) = fs::read_dir(dir) else { return };

    for entry in entries.flatten() {
        let path = entry.path();

        // Skip static/ — served by tower_http::ServeDir, not by page routes
        if path.is_dir() {
            if let Ok(rel) = path.strip_prefix(build_dir) {
                if rel == Path::new("static") {
                    continue;
                }
            }
            walk_html_files(&path, build_dir, pages);
            continue;
        }

        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }

        let Ok(content) = fs::read_to_string(&path) else { continue };
        let Ok(rel) = path.strip_prefix(build_dir) else { continue };

        // Normalize Windows backslashes to forward slashes
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // Strip .html suffix → gives us the route-shaped path
        let no_ext = rel_str.trim_end_matches(".html");

        // Route key derivation:
        //   "index"          → "/"
        //   "admin/index"    → "/admin"
        //   "admin/users"    → "/admin/users"
        let key = if no_ext == "index" {
            "/".to_string()
        } else if let Some(stripped) = no_ext.strip_suffix("/index") {
            format!("/{}", stripped)
        } else {
            format!("/{}", no_ext)
        };

        pages.insert(key, Arc::from(content));
    }
}

static BUILD_CACHE: OnceLock<RwLock<Arc<BuildCache>>> = OnceLock::new();

#[inline]
fn cache_cell() -> &'static RwLock<Arc<BuildCache>> {
    BUILD_CACHE.get_or_init(|| RwLock::new(Arc::new(BuildCache::empty())))
}

fn ensure_build_cache(build_dir: &Path) -> Arc<BuildCache> {
    {
        let guard = cache_cell().read().unwrap();
        if guard.initialized {
            return Arc::clone(&guard);
        }
    }
    let fresh = Arc::new(BuildCache::load(build_dir));
    *cache_cell().write().unwrap() = Arc::clone(&fresh);
    fresh
}

fn invalidate_build_cache() {
    *cache_cell().write().unwrap() = Arc::new(BuildCache::empty());
    crate::state::clear_html_render_cache();
}

// ─────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────
#[derive(Subcommand)]
pub enum Commands {
    #[command(name = "init", alias = "new")]
    Init {
        #[arg(default_value = ".")]
        name: String,
        #[arg(short, long, default_value = "sqlite", value_parser = ["sqlite", "postgres", "mysql"])]
        db: String,
        #[arg(long = "db-name", alias = "db_name")]
        db_name: Option<String>,
        #[arg(long)]
        no_db: bool,
    },

    Dev {
        #[arg(short, long)]
        port: Option<u16>,
        #[arg(long)]
        host: Option<String>,
    },

    Build {
        #[arg(long)]
        release: bool,
    },

    Serve {
        #[arg(short, long)]
        port: Option<u16>,
        #[arg(long)]
        host: Option<String>,
    },

    /// Print or write schema.sql translated for a database driver
    Schema {
        #[arg(short, long, default_value = "sqlite", value_parser = ["sqlite", "postgres", "mysql"])]
        driver: String,
        #[arg(short, long)]
        out: Option<String>,
    },

    /// Validate templates, components, layouts, modules and API actions
    Check {
        /// Emit a machine-readable JSON report
        #[arg(long)]
        json: bool,
        /// Fail on warnings as well as errors
        #[arg(long)]
        strict: bool,
    },

    Cgi,

    Deploy {
        #[arg(
            short,
            long,
            default_value = "netlify",
            value_parser = ["netlify", "vercel", "cloudflare", "pages", "railway"]
        )]
        provider: String,
    },
}

// ─────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────
async fn healthz_handler() -> impl IntoResponse {
    let db_enabled = is_enabled(Feature::Database);
    let db_connected = db_enabled && crate::database::DB_POOL.get().is_some();

    let status = if !db_enabled {
        "healthy (no db)"
    } else if db_connected {
        "healthy"
    } else {
        "degraded"
    };

    let http_status = if status == "degraded" {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    (
        http_status,
        axum::Json(serde_json::json!({
            "status": status,
            "uptime_seconds": crate::state::uptime_seconds(),
            "database": if !db_enabled { "disabled" } else if db_connected { "connected" } else { "disconnected" },
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

async fn broadcast_handler(
    axum::Extension(auth): axum::Extension<crate::auth::AuthUser>,
    connect_info: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> Response {
    let authorized = if is_enabled(Feature::Auth) {
        auth.user.as_ref().is_some_and(|u| u.role == "Admin")
    } else {
        connect_info.is_some_and(|axum::extract::ConnectInfo(addr)| addr.ip().is_loopback())
    };
    if !authorized {
        return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "success": false }))).into_response();
    }

    let channel = payload.get("channel").and_then(|v| v.as_str()).unwrap_or("default");
    let value = payload.get("value").cloned().unwrap_or(serde_json::Value::Null);
    crate::router::broadcast_live(channel, &value);
    (StatusCode::OK, axum::Json(serde_json::json!({ "success": true }))).into_response()
}

// ─────────────────────────────────────────────────────────────
// Middleware
// ─────────────────────────────────────────────────────────────
async fn request_id_middleware(mut req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(crate::state::generate_request_id);

    req.extensions_mut().insert(crate::state::RequestId(request_id.clone()));
    let mut response = next.run(req).await;

    if let Ok(val) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", val);
    }
    response
}

#[inline]
fn has_immutable_extension(path: &str) -> bool {
    const EXTS: &[&str] = &[
        ".css", ".js", ".mjs", ".cjs", ".png", ".jpg", ".jpeg", ".gif",
        ".webp", ".svg", ".ico", ".woff", ".woff2", ".ttf", ".otf", ".map",
    ];
    let last_seg = path.rsplit('/').next().unwrap_or(path);
    let Some(dot) = last_seg.rfind('.') else { return false };
    let ext = &last_seg[dot..];
    EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

async fn security_headers_middleware(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers
        .entry(HeaderName::from_static("x-content-type-options"))
        .or_insert(HeaderValue::from_static("nosniff"));
    headers
        .entry(HeaderName::from_static("x-frame-options"))
        .or_insert(HeaderValue::from_static("DENY"));
    headers
        .entry(HeaderName::from_static("referrer-policy"))
        .or_insert(HeaderValue::from_static("strict-origin-when-cross-origin"));
    headers
        .entry(HeaderName::from_static("permissions-policy"))
        .or_insert(HeaderValue::from_static(
            "camera=(), microphone=(), geolocation=()",
        ));
    headers
        .entry(HeaderName::from_static("content-security-policy"))
        .or_insert(HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: blob: https:; font-src 'self' data:; media-src 'self' blob:; \
             connect-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; \
             frame-ancestors 'none'",
        ));
    response
}

async fn cache_middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let is_sse = path.starts_with("/__vlo_sse");
    let is_static = path.starts_with("/static/")
        || path.starts_with("/uploads/")
        || path.starts_with("/storage/");
    let is_immutable_ext = !is_static && has_immutable_extension(path);

    let mut response = next.run(req).await;

    if is_sse {
        response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response.headers_mut().insert(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        );
        return response;
    }

    // 🔥 Don't clobber an explicit Cache-Control set by the handler
    //    (e.g. Live-tier pages return "no-store, private").
    if response.headers().contains_key(CACHE_CONTROL) {
        return response;
    }

    let value = if is_static || is_immutable_ext {
        "public, max-age=31536000, immutable"
    } else {
        "private, no-cache"
    };
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

// ─────────────────────────────────────────────────────────────
// Background tasks
// ─────────────────────────────────────────────────────────────
async fn rate_limit_cleanup_task() {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
        crate::auth::cleanup_rate_limits();
        crate::vlo_debug!("🧹 Rate limit cleanup completed");
    }
}

async fn session_cleanup_task() {
    match crate::auth::cleanup_expired_sessions().await {
        Ok(n) if n > 0 => crate::vlo_debug!("🧹 Startup session cleanup: purged {} sessions", n),
        Err(e) => crate::vlo_debug!("⚠️ Startup session cleanup failed: {}", e),
        _ => {}
    }
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        match crate::auth::cleanup_expired_sessions().await {
            Ok(n) if n > 0 => crate::vlo_debug!("🧹 Hourly session cleanup: purged {} sessions", n),
            Err(e) => crate::vlo_debug!("⚠️ Hourly session cleanup failed: {}", e),
            _ => {}
        }
    }
}

// ─────────────────────────────────────────────────────────────
// Router construction
// ─────────────────────────────────────────────────────────────
fn build_common_routes() -> Router {
    let mut app = Router::new()

        // 🔥 MUST BE BEFORE GENERIC API ROUTES
        .route("/__vlo/image", get(crate::files_api::optimize_image))
        .route("/uploads/*path", get(serve_file))
        .route("/healthz", get(healthz_handler))
        // Legacy/entry URL used by older nav links and redirects.
        .route("/dashboard", get(|| async { Redirect::to("/admin/dashboard") }))
        .route("/__vlo/ajax.js", get(crate::router::ajax_js_handler))

        // 🔥 GENERIC FILE API — admin-only, whitelisted dirs only.
        //    Must come BEFORE /api/:resource so `_files` isn't
        //    treated as a resource name. Static `_files` beats
        //    dynamic `:resource` in Axum, but explicit ordering
        //    documents the intent and survives future route shuffles.
        .route(
            "/api/_files",
            get(crate::fs_api::list_files),
        )
        .route(
            "/api/_files/*path",
            get(crate::fs_api::read_file)
                .put(crate::fs_api::write_file)
                .delete(crate::fs_api::delete_file),
        );

    if is_enabled(Feature::Files) {
        app = app
            .route("/api/files/upload", axum::routing::post(crate::files_api::upload_file))
            .route("/api/files/:id/download", get(crate::files_api::download_file))
            .route("/api/files/:id", get(crate::files_api::get_file).delete(crate::files_api::delete_file))
            .route("/storage/*path", get(crate::files_api::serve_file));
    }
    if is_enabled(Feature::Auth) {
        app = app
            .route("/api/auth/login", axum::routing::post(auth::login_handler))
            .route(
                "/api/auth/logout",
                axum::routing::post(auth::logout_handler),
            )
            .route("/api/auth/me", axum::routing::get(auth::me_handler))
            .route("/api/auth/register", axum::routing::post(auth::register_handler))
            .route("/api/auth/activate", axum::routing::get(auth::activate_handler))
            .route("/api/auth/resend-activation", axum::routing::post(auth::resend_activation_handler))
            .route("/api/auth/forgot-password", axum::routing::post(auth::forgot_password_handler))
            .route("/api/auth/reset-password", axum::routing::post(auth::reset_password_handler));
    }

    if is_enabled(Feature::Sse) {
        app = app
            .route("/__vlo_sse", get(crate::router::sse_handler))
            .route("/api/broadcast", axum::routing::post(broadcast_handler));
    }

    if is_enabled(Feature::Api) {
        // Inlined (not generic) — `Request<Body>` is not Clone/Sync, so a
        // generic wrapper requires bounds that Axum's handler state can't meet.
        app = app
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
            );
    }

    crate::vlo_debug!(
        "🚀 [ROUTER] Routes built with {} features enabled",
        crate::features::enabled_features().len()
    );
    app
}

fn apply_common_layers(app: Router) -> Router {
    let mut app = app
        .layer(DefaultBodyLimit::max(25 * 1024 * 1024))
        .layer(CompressionLayer::new().compress_when(NotForContentType::new("text/event-stream")))
        .layer(middleware::from_fn(cache_middleware))
        .layer(middleware::from_fn(security_headers_middleware))
        .layer(middleware::from_fn(request_id_middleware));

    if is_enabled(Feature::Auth) {
        crate::vlo_debug!("🔐 [MIDDLEWARE] Auth (Session → API Auth → CSRF)");
        app = app.layer(middleware::from_fn(auth::csrf_middleware));
        if is_enabled(Feature::Api) {
            app = app.layer(middleware::from_fn(auth::api_auth_middleware));
        }
        app = app.layer(middleware::from_fn(auth::session_middleware));
    } else {
        // Empty AuthUser so extensions lookups never panic.
        app = app.layer(middleware::from_fn(|mut req: Request, next: Next| async move {
            req.extensions_mut().insert(crate::auth::AuthUser {
                user: None,
                csrf_token: None,
                session_token: None,
            });
            next.run(req).await
        }));
    }

    crate::vlo_debug!(
        "🚀 [MIDDLEWARE] Applied with {} features enabled",
        crate::features::enabled_features().len()
    );
    app
}

// ─────────────────────────────────────────────────────────────
// Server bootstrap
// ─────────────────────────────────────────────────────────────
async fn run_server(
    mode: state::AppMode,
    app: Router,
    host: Option<&str>,
    port: Option<u16>,
    server_type: &str,
) -> Result<(), String> {
    state::set_app_mode(mode);

    if is_enabled(Feature::Sse) {
        crate::router::init_live_broadcast();
    }

    let host_str = host
        .map(str::to_owned)
        .or_else(|| std::env::var("HOST").ok())
        .or_else(|| std::env::var("VLO_HOST").ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let port_num = port
        .or_else(|| std::env::var("PORT").ok().and_then(|v| v.parse().ok()))
        .or_else(|| std::env::var("VLO_PORT").ok().and_then(|v| v.parse().ok()))
        .unwrap_or(3000);

    let addr = format!("{}:{}", host_str, port_num);

    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        format!(
            "Failed to start VLO {} server.\n   Address: {}\n   Error: {}\n   Try: vlo {} --port 3001",
            server_type, addr, e, server_type
        )
    })?;

    println!("⚡ VLO {} server: http://{}", server_type, addr);

    if is_enabled(Feature::Auth) {
        tokio::spawn(rate_limit_cleanup_task());
        tokio::spawn(session_cleanup_task());
    }

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|e| format!("VLO {} server stopped unexpectedly.\n   Error: {}", server_type, e))
}

pub async fn dev(host: Option<&str>, port: Option<u16>) -> Result<(), String> {
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

    let app = build_common_routes()
        .route("/", get(home_handler))
        .route("/:path", get(page_handler))
        .route("/__vlo_hmr", get(move || hmr_handler(tx)))
        .nest_service("/static", ServeDir::new(public_path_service))
        .fallback(page_handler_any);

    let app = apply_common_layers(app);
    run_server(state::AppMode::Development, app, host, port, "dev").await
}

pub async fn serve(host: Option<&str>, port: Option<u16>) -> Result<(), String> {
    let root = get_project_root();
    let build_dir = root.join(".vlo").join("build");
    let static_dir = build_dir.join("static");

    if !build_dir.exists() {
        return Err("Production build not found.\n   Run `vlo build` first.".to_string());
    }

    // Warm cache on startup — first request no longer blocks on disk I/O.
    ensure_build_cache(&build_dir);

    let app = build_common_routes()
        .nest_service("/static", ServeDir::new(static_dir))
        .fallback(move |uri: Uri, req: Request| {
            let build_dir = build_dir.clone();
            async move { serve_build_page(build_dir, uri, req).await }
        });

    let app = apply_common_layers(app);
    run_server(state::AppMode::Production, app, host, port, "production").await
}

// ─────────────────────────────────────────────────────────────
// CGI mode
// ─────────────────────────────────────────────────────────────
fn print_cgi_404() {
    println!("Status: 404 Not Found");
    println!("Content-Type: text/html; charset=utf-8");
    println!();
    println!("<h1>404</h1><p>Page Not Found</p>");
}

pub async fn cgi() -> Result<(), String> {
    let root = crate::state::get_project_root();

    let request_uri = std::env::var("REQUEST_URI").unwrap_or_else(|_| "/".to_string());
    let path = request_uri.split('?').next().unwrap_or("/").trim_matches('/');

    if path.ends_with(".vlo") || path.starts_with("pages/") {
        print_cgi_404();
        return Ok(());
    }

    let page_name = if path.is_empty() {
        "index".to_string()
    } else {
        path.to_string()
    };

    if page_name.contains("..") || page_name.contains('\\') {
        print_cgi_404();
        return Ok(());
    }

    let page_file = root.join("pages").join(format!("{}.vlo", page_name));
    let query = parse_cgi_query(&std::env::var("QUERY_STRING").unwrap_or_default());

    match fs::read_to_string(&page_file) {
        Ok(source) => {
            let rendered = crate::router::render_vlo_with_query(source, &query);
            let html = crate::router::wrap_html(&page_name, &rendered, true);
            println!("Status: 200 OK");
            println!("Content-Type: text/html; charset=utf-8");
            println!();
            print!("{}", html);
        }
        Err(_) => {
            let response = crate::router::render_404().await.into_response();
            let status = response.status();
            println!(
                "Status: {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Not Found")
            );
            println!("Content-Type: text/html; charset=utf-8");
            println!();

            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .map_err(|e| format!("Failed to read CGI response: {}", e))?;
            print!("{}", String::from_utf8_lossy(&body));
        }
    }
    Ok(())
}

fn parse_cgi_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────
// Production page rendering helpers
// ─────────────────────────────────────────────────────────────

/// Parse query string once; return both the map and the `next` param.
fn parse_query(uri: &Uri) -> (HashMap<String, String>, Option<String>) {
    let mut query = HashMap::new();
    let mut next = None;

    let Some(q) = uri.query() else {
        return (query, next);
    };
    if q.is_empty() {
        return (query, next);
    }

    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, raw_v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = urlencoding::decode(raw_v)
            .map(Cow::into_owned)
            .unwrap_or_else(|_| raw_v.to_string());
        if k == "next" {
            next = Some(v.clone());
        }
        query.insert(k.to_string(), v);
    }
    (query, next)
}

/// Resolve auth from already-extracted values. Never borrows `&Request`,
/// so the returned future is `Send` and works in fallback closures.
async fn resolve_auth(
    existing: Option<crate::auth::AuthUser>,
    cookie_header: Option<&str>,
) -> Option<crate::auth::AuthUser> {
    if !is_enabled(Feature::Auth) {
        return None;
    }

    if let Some(a) = existing {
        if a.user.is_some() {
            return Some(a);
        }
    }

    let cookie_header = cookie_header?;
    let cookie_name = crate::auth::auth_config().cookie_name.clone();
    let token = crate::auth::parse_cookie_header(cookie_header, &cookie_name)?;
    let user = crate::auth::get_user_from_session(&token).await?;

    Some(crate::auth::AuthUser {
        user: Some(user),
        csrf_token: Some(crate::auth::compute_csrf_token(&token)),
        session_token: Some(token),
    })
}

/// Extract the flash message (if any) from an owned cookie header string.
fn extract_flash(cookie_header: Option<&str>) -> (Option<serde_json::Value>, String, bool) {
    if !is_enabled(Feature::Flash) {
        return (None, String::new(), false);
    }

    let Some(header) = cookie_header else {
        return (None, String::new(), false);
    };
    let Some(encoded) = crate::auth::parse_cookie_header(header, crate::auth::FLASH_COOKIE) else {
        return (None, String::new(), false);
    };
    let Some(flash) = crate::auth::decode_flash(&encoded) else {
        return (None, String::new(), false);
    };

    let variant = flash.get("variant").and_then(|v| v.as_str()).unwrap_or("success");
    let icon = crate::template::escape_html_text(
        flash.get("icon").and_then(|v| v.as_str()).unwrap_or("✅"),
    );
    let title = crate::template::escape_html_text(
        flash.get("title").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let description = crate::template::escape_html_text(
        flash.get("description").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let border_color = match variant {
        "success" => "#00ff88",
        "error" => "#ff4444",
        "warning" => "#ffaa00",
        _ => "#00f5ff",
    };

    let html = format!(
        r#"<div class="vlo-flash" style="position:fixed;top:20px;right:20px;z-index:99999;padding:16px 22px;border-radius:10px;background:#1a1a2e;border-left:4px solid {};color:#fff;box-shadow:0 8px 24px rgba(0,0,0,0.4);display:flex;align-items:center;gap:12px;max-width:380px;animation:vloFlashIn 0.35s ease"><span style="font-size:1.4rem">{}</span><div><strong style="display:block;font-size:0.9rem;margin-bottom:2px">{}</strong><span style="color:#aaa;font-size:0.78rem">{}</span></div></div><style>@keyframes vloFlashIn{{from{{opacity:0;transform:translateX(40px)}}to{{opacity:1;transform:translateX(0)}}}}</style>"#,
        border_color, icon, title, description
    );

    (Some(flash), html, true)
}

fn invalid_manifest_response() -> Response {
    eprintln!("[VLO] routes.json contains an invalid guard entry — refusing to serve.");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html("500 - Invalid build manifest. Run `vlo build`.".to_string()),
    )
        .into_response()
}

/// Missing keys default to false; present-but-wrong-typed keys fail closed.
fn manifest_bool(cfg: &serde_json::Value, key: &str) -> Result<bool, ()> {
    match cfg.get(key) {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or(()),
    }
}

fn check_guard(
    cache: &BuildCache, path: &str, auth: &Option<crate::auth::AuthUser>, next: Option<&str>,
) -> Option<Response> {
    let cfg = cache.manifest.as_object()?.get(path)?;
    if !cfg.is_object() {
        return Some(invalid_manifest_response());
    }

    let req_auth = match manifest_bool(cfg, "auth") {
        Ok(v) => v,
        Err(_) => return Some(invalid_manifest_response()),
    };

    if !is_enabled(Feature::Auth) {
        return req_auth.then(|| (StatusCode::NOT_FOUND, Html("404 - Page Not Found".to_string())).into_response());
    }

    let guest = match manifest_bool(cfg, "guest") {
        Ok(v) => v,
        Err(_) => return Some(invalid_manifest_response()),
    };
    let roles = match cfg.get("roles") {
        None => Vec::new(),
        Some(serde_json::Value::Array(items)) => {
            let mut roles = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(role) => roles.push(role.to_string()),
                    None => return Some(invalid_manifest_response()),
                }
            }
            roles
        }
        Some(_) => return Some(invalid_manifest_response()),
    };

    let guard = crate::auth::PageGuard { auth: req_auth, guest, roles };

    crate::auth::check_page_guard(&guard, &auth.as_ref().and_then(|a| a.user.clone()), path, next)
}

fn inject_auth_context(
    ctx: &mut HashMap<String, serde_json::Value>,
    auth: &Option<crate::auth::AuthUser>,
) {
    if !is_enabled(Feature::Auth) {
        return;
    }
    let cfg = crate::auth::auth_config();
    ctx.insert("auth_identifier_field".into(), serde_json::Value::String(cfg.identifier_field.clone()));
    ctx.insert("auth_password_field".into(), serde_json::Value::String(cfg.password_field.clone()));

    if let Some(user) = auth.as_ref().and_then(|a| a.user.as_ref()) {
        ctx.insert("logged_in".into(), serde_json::Value::Bool(true));
        ctx.insert("user_id".into(), serde_json::Value::Number(user.id.into()));
        ctx.insert("user_name".into(), serde_json::Value::String(user.name.clone()));
        ctx.insert("user_role".into(), serde_json::Value::String(user.role.clone()));
        ctx.insert("user_email".into(), serde_json::Value::String(user.email.clone()));
    }
}

fn inject_flash_context(
    ctx: &mut HashMap<String, serde_json::Value>,
    flash: &Option<serde_json::Value>,
) {
    if !is_enabled(Feature::Flash) {
        return;
    }
    let Some(flash) = flash else { return };

    ctx.insert("flash_messages".into(), serde_json::json!([flash]));

    let Some(obj) = flash.as_object() else { return };
    for (k, v) in obj {
        ctx.insert(format!("flash_{}", k), v.clone());
    }
    for key in ["variant", "icon", "title", "description"] {
        if let Some(v) = obj.get(key) {
            ctx.insert(key.into(), v.clone());
        }
    }
    let desc_len = obj
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().count())
        .unwrap_or(0);
    let duration = if desc_len < 30 { "short" } else if desc_len < 80 { "medium" } else { "long" };
    ctx.insert("duration".into(), serde_json::Value::String(duration.into()));
}

fn inject_pagination_context(ctx: &mut HashMap<String, serde_json::Value>) {
    if !is_enabled(Feature::Pagination) {
        return;
    }
    ctx.entry("limit".into()).or_insert_with(|| serde_json::Value::Number(20.into()));
    ctx.entry("page".into()).or_insert_with(|| serde_json::Value::Number(1.into()));
    ctx.entry("order".into()).or_insert_with(|| serde_json::Value::String("asc".into()));

    let p = ctx.get("page").and_then(|v| v.as_i64()).unwrap_or(1).max(1);
    let l = ctx.get("limit").and_then(|v| v.as_i64()).unwrap_or(20).max(1);

    ctx.insert("offset".into(), serde_json::Value::Number(((p - 1) * l).into()));
    ctx.insert("prev_page".into(), serde_json::Value::Number((p - 1).max(1).into()));
    ctx.insert("next_page".into(), serde_json::Value::Number((p + 1).into()));
}

fn inject_sse_script(final_html: &mut String, auth: &Option<crate::auth::AuthUser>) {
    if !is_enabled(Feature::Sse) {
        return;
    }
    let has_markers =
        final_html.contains("class=\"vlo-sync\"") || final_html.contains("data-channel=");
    if !has_markers {
        return;
    }

    let csrf = if is_enabled(Feature::Auth) {
        auth.as_ref().and_then(|a| a.csrf_token.as_deref()).unwrap_or("")
    } else {
        ""
    };

    let script: Cow<str> = if csrf.is_empty() {
        Cow::Borrowed(SSE_SCRIPT)
    } else {
        Cow::Owned(SSE_SCRIPT.replace("__CSRF_TOKEN__", csrf))
    };

    if let Some(start) = final_html.find(r#"<script id="__VLO_SSE__">"#) {
        if let Some(rel_end) = final_html[start..].find("</script>") {
            let end = start + rel_end + "</script>".len();
            final_html.replace_range(start..end, &script);
            return;
        }
    }
    if let Some(i) = final_html.rfind("</body>") {
        final_html.insert_str(i, &script);
        final_html.insert(i + script.len(), '\n');
    }
}

// ---------------------------------------------------------------------------
// Cache Policy Parsing & Helpers
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq)]
struct CachePolicy {
    tier: CacheTier,
    browser_ttl: u64, // Seconds
    server_ttl: u64,  // Seconds
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CacheTier {
    Static, // Truly static pages (About, Pricing)
    User,   // User-specific pages (Dashboard, Profile)
    Live,   // Live DB data (Products, Orders)
    Auto,   // Auto-detect from content
}

/// Parses strings like "static", "user:7200", or "static:7200:86400"
fn parse_cache_policy(raw: &str) -> CachePolicy {
    let parts: Vec<&str> = raw.split(':').collect();
    let tier_str = parts.get(0).map(|s| *s).unwrap_or("auto");
    
    let tier = match tier_str.trim() {
        "static" => CacheTier::Static,
        "user" => CacheTier::User,
        "live" => CacheTier::Live,
        _ => CacheTier::Auto,
    };

    let default_browser = match tier {
        CacheTier::Static => 3600, // 1 hour default
        CacheTier::User => 0,      // no browser cache default
        CacheTier::Live => 0,
        CacheTier::Auto => 0,
    };
    
    let default_server = if matches!(tier, CacheTier::Live) { 0 } else { 60 }; // 60s default

    let browser_ttl = parts.get(1)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default_browser);

    let server_ttl = parts.get(2)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default_server);

    CachePolicy { tier, browser_ttl, server_ttl }
}

const HTML_RENDER_CACHE_MAX: usize = 512;

fn apply_cache_headers(response: &mut Response, policy: &CachePolicy, personalized: bool) {
    let cache_control = match policy.tier {
        CacheTier::Static => {
            if policy.browser_ttl > 0 {
                format!("public, max-age={}, stale-while-revalidate=86400", policy.browser_ttl)
            } else {
                "public, no-cache".to_string()
            }
        }
        CacheTier::User => {
            if policy.browser_ttl > 0 {
                format!("private, max-age={}, must-revalidate", policy.browser_ttl)
            } else {
                "private, no-cache, must-revalidate".to_string()
            }
        }
        CacheTier::Live => "no-store, private".to_string(),
        CacheTier::Auto => "private, no-store".to_string(),
    };
    // Personalized HTML (logged-in nav, flash toast, query-driven output) must
    // never enter a shared/public cache.
    let cache_control = if personalized && cache_control.starts_with("public") {
        "private, no-cache, must-revalidate".to_string()
    } else {
        cache_control
    };

    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_str(&cache_control).unwrap(),
    );
}

fn inject_per_request_dynamic(html: &mut String, auth: &Option<crate::auth::AuthUser>, flash_html: &str) {
    // 1. CSRF
    if html.contains("__VLO_CSRF_PLACEHOLDER__") {
        let csrf = if is_enabled(Feature::Auth) {
            auth.as_ref().and_then(|a| a.csrf_token.as_deref()).unwrap_or("")
        } else {
            ""
        };
        *html = html.replace("__VLO_CSRF_PLACEHOLDER__", csrf);
    }

    // 2. Flash
    if is_enabled(Feature::Flash)
        && !flash_html.is_empty()
        && html.contains(r#"<div id="__VLO_FLASH_PLACEHOLDER__"></div>"#)
    {
        *html = html.replace(r#"<div id="__VLO_FLASH_PLACEHOLDER__"></div>"#, flash_html);
    }

    // 3. SSE
    inject_sse_script(html, auth);

    // 4. AJAX core for targets that only exist after runtime control-flow
    //    rendering (e.g. shop cart). Build-time pages already carry it.
    if html.contains("__VLO_AJAX_Q") && !html.contains("__VLO_AJAX_READY") {
        let mut scripts = format!("<script>{}</script>", crate::modifier::AJAX_CORE_BASE);
        if html.contains("data-load-more") || html.contains("data-append") {
            scripts.push_str(&format!(
                "\n<script>{}</script>",
                crate::modifier::AJAX_CORE_LOADMORE
            ));
        }
        if let Some(i) = html.rfind("</body>") {
            html.insert_str(i, &scripts);
        }
    }
}
// ─────────────────────────────────────────────────────────────
// serve_build_page — production page renderer with HTML caching
// ─────────────────────────────────────────────────────────────
async fn serve_build_page(build_dir: PathBuf, uri: Uri, req: Request) -> Response {
    let cache = ensure_build_cache(&build_dir);

    // Fail closed: a missing or unparsable routes.json must never silently
    // disable page guards. Rebuild to regenerate it.
    if is_enabled(Feature::Auth) && !cache.manifest_trusted {
        eprintln!("[VLO] routes.json is missing or invalid — refusing to serve guarded pages. Run `vlo build`.");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("500 - Build manifest missing or invalid. Run `vlo build`.".to_string()),
        )
            .into_response();
    }

    // 🔥 Normalize trailing slash: /admin/ → /admin (but keep "/" as "/")
    let raw_path = uri.path();
    let path: &str = if raw_path == "/" {
        "/"
    } else {
        raw_path.trim_end_matches('/')
    };
    // Alias keeps downstream references unchanged
    let lookup = path;

    let (query, next_decoded) = parse_query(&uri);

    let existing_auth = req.extensions().get::<crate::auth::AuthUser>().cloned();
    let cookie_header = req
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let auth = resolve_auth(existing_auth, cookie_header.as_deref()).await;
    let (flash_value, flash_html, expire_cookie) = extract_flash(cookie_header.as_deref());

    if let Some(mut resp) = check_guard(&cache, path, &auth, next_decoded.as_deref()) {
        if expire_cookie {
            if let Ok(v) = HeaderValue::from_str(&crate::auth::expire_flash_cookie()) {
                resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
            }
        }
        return resp;
    }

    // `lookup` is now canonical — no need for the old if-else
    let Some(page_html) = cache.pages.get(lookup).cloned() else {
        let not_found = build_dir.join("404.html");
        return match fs::read_to_string(not_found) {
            Ok(html) => (StatusCode::NOT_FOUND, Html(html)).into_response(),
            Err(_) => (StatusCode::NOT_FOUND, Html("404 - Page Not Found".to_string())).into_response(),
        };
    };

    // ─── 1. Determine Cache Policy from Manifest ───
    let cache_policy = cache.manifest.as_object()
        .and_then(|routes| routes.get(lookup))
        .and_then(|cfg| cfg.get("cache"))
        .and_then(|v| v.as_str())
        .map(parse_cache_policy)
        .unwrap_or_else(|| {
            // Fallback: auto-detect from content
            if page_html.contains("data-source") {
                CachePolicy { tier: CacheTier::Live, browser_ttl: 0, server_ttl: 0 }
            } else {
                CachePolicy { tier: CacheTier::Static, browser_ttl: 3600, server_ttl: 60 }
            }
        });

    // Cache key includes the full identity (id + role) and the query string so
    // personalized HTML and query-driven pages can never collide.
    let user_key = auth.as_ref()
        .and_then(|a| a.user.as_ref())
        .map(|u| format!("{}:{}", u.id, u.role))
        .unwrap_or_else(|| "guest".to_string());
    let raw_query = uri.query().unwrap_or("");
    let query_cacheable = raw_query.len() <= 512;
    let query_key = if raw_query.is_empty() {
        String::new()
    } else if query_cacheable {
        format!("?{}", raw_query)
    } else {
        "?__oversized__".to_string()
    };
    let cache_key = format!("{}|{}|{}", lookup, user_key, query_key);
    let cacheable = cache_policy.server_ttl > 0 && query_cacheable;
    let personalized = auth.as_ref().and_then(|a| a.user.as_ref()).is_some()
        || flash_value.is_some()
        || !raw_query.is_empty();

    // ─── 2. Try Server Cache ───
    if cacheable {
        if let Ok(cache_guard) = crate::state::HTML_RENDER_CACHE.lock() {
            if let Some(entry) = cache_guard.get(&cache_key) {
                if entry.created_at.elapsed() < entry.ttl {
                    let mut cached_html = entry.html.clone();
                    inject_per_request_dynamic(&mut cached_html, &auth, &flash_html);
                    
                    let mut response = (StatusCode::OK, Html(cached_html)).into_response();
                    apply_cache_headers(&mut response, &cache_policy, personalized);
                    
                    if expire_cookie && is_enabled(Feature::Flash) {
                        if let Ok(v) = HeaderValue::from_str(&crate::auth::expire_flash_cookie()) {
                            response.headers_mut().append(axum::http::header::SET_COOKIE, v);
                        }
                    }
                    return response;
                }
            }
        }
    }

    // ─── 3. Full Render Pipeline (Cache Miss or Non-Cacheable) ───
    let mut context: HashMap<String, serde_json::Value> = HashMap::with_capacity(query.len() + 12);
    for (k, v) in &query {
        let value = match v.parse::<i64>() {
            Ok(n) => serde_json::Value::Number(n.into()),
            Err(_) => serde_json::Value::String(v.clone()),
        };
        context.insert(k.clone(), value);
    }

    inject_auth_context(&mut context, &auth);
    inject_flash_context(&mut context, &flash_value);
    inject_pagination_context(&mut context);

    let (html_with_data, computed) = if is_enabled(Feature::Database) && page_html.contains("data-source") {
        crate::router::resolve_data_sources(&page_html, &context)
    } else {
        (page_html.to_string(), HashMap::new())
    };
    context.extend(computed);

    let mut page = crate::state::RenderedPage {
        html: html_with_data,
        styles: Vec::new(),
        template_context: context,
        used_modules: HashSet::new(),
    };

    for _ in 0..20 {
        let previous = std::mem::take(&mut page.html);
        let current = previous.clone();
        page.html = crate::component::render_components(&current, &mut page);
        if page.html == previous {
            if page.html.is_empty() { page.html = previous; }
            break;
        }
    }

    let mut pre_injection_html = crate::template::render_control_flow(&page.html, &page.template_context);

    // ─── 4. Write to Server Cache ───
    if cacheable {
        if let Ok(mut cache_guard) = crate::state::HTML_RENDER_CACHE.lock() {
            cache_guard.retain(|_, entry| entry.created_at.elapsed() < entry.ttl);
            if cache_guard.len() < HTML_RENDER_CACHE_MAX {
                cache_guard.insert(cache_key, crate::state::CachedHtml {
                    html: pre_injection_html.clone(),
                    created_at: std::time::Instant::now(),
                    ttl: std::time::Duration::from_secs(cache_policy.server_ttl),
                });
            }
        }
    }

    // ─── 5. Inject Dynamic Parts & Return ───
    inject_per_request_dynamic(&mut pre_injection_html, &auth, &flash_html);
    
    let mut response = (StatusCode::OK, Html(pre_injection_html)).into_response();
    apply_cache_headers(&mut response, &cache_policy, personalized);

    if expire_cookie && is_enabled(Feature::Flash) {
        if let Ok(v) = HeaderValue::from_str(&crate::auth::expire_flash_cookie()) {
            response.headers_mut().append(axum::http::header::SET_COOKIE, v);
        }
    }
    
    response
}


async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");
    println!("\n⚡ Shutting down VLO dev server...");
}

// ─────────────────────────────────────────────────────────────
// Build
// ─────────────────────────────────────────────────────────────
struct BuildFlagGuard;
impl Drop for BuildFlagGuard {
    fn drop(&mut self) {
        crate::state::set_building(false);
    }
}

pub fn build(release: bool) -> Result<(), String> {
    crate::state::set_building(true);
    let _guard = BuildFlagGuard;

    println!(
        "⚡ VLO {} build...",
        if release { "release" } else { "production" }
    );

    let root = get_project_root();
    let pages = root.join("pages");
    let public = root.join("public");
    let build_dir = root.join(".vlo").join("build");

    if build_dir.exists() {
        fs::remove_dir_all(&build_dir)
            .map_err(|e| format!("Failed to clean previous build: {}", e))?;
    }
    fs::create_dir_all(build_dir.join("static"))
        .map_err(|e| format!("Failed to create build dir: {}", e))?;

    let mut routes_manifest = serde_json::Map::new();
    build_pages_recursive(&pages, &pages, &build_dir, &mut routes_manifest)?;

    // Always write routes.json — serve treats a missing/invalid manifest as
    // fail-closed, so its presence is part of the build contract.
    let count = routes_manifest.len();
    let manifest_path = build_dir.join("routes.json");
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(routes_manifest))
        .map_err(|e| format!("Failed to serialize routes manifest: {}", e))?;
    fs::write(&manifest_path, json)
        .map_err(|e| format!("Failed to write routes manifest: {}", e))?;
    println!("  ├─ Routes manifest: {} guards", count);

    if public.exists() {
        copy_dir_all(&public, &build_dir.join("static"))
            .map_err(|e| format!("Copy static assets: {}", e))?;
        println!("  └─ Copied static assets");
    }

    // Invalidate live cache so `vlo serve` picks up the fresh build.
    invalidate_build_cache();

    println!("⚡ Build completed successfully!");
    Ok(())
}

fn build_pages_recursive(
    dir: &Path, pages_root: &Path, build_dir: &Path, manifest: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("Failed to read dir: {}", e))?.flatten() {
        let path = entry.path();
        if path.is_dir() { 
            // 🔥 Skip the entire pages/api/ subtree — API definitions are
            //    loaded by `load_api_actions`, never rendered as HTML pages.
            if path == pages_root.join("api") {
                continue;
            }
            build_pages_recursive(&path, pages_root, build_dir, manifest)?; 
            continue; 
        }
        if path.extension().and_then(|e| e.to_str()) != Some("vlo") { continue; }

        // rel: file path relative to pages/ (used for layout resolution + output path)
        //   pages/index.vlo         → "index"
        //   pages/admin/index.vlo   → "admin/index"
        //   pages/blog/post.vlo     → "blog/post"
        let rel = path.strip_prefix(pages_root).map_err(|e| format!("Path error: {}", e))?
            .with_extension("").to_string_lossy().replace('\\', "/");

        // route: URL path this page responds to (used for manifest key + title fallback)
        //   "index"          → "/"
        //   "admin/index"    → "/admin"
        //   "blog/post"      → "/blog/post"
        let route = if rel == "index" {
            "/".to_string()
        } else if let Some(stripped) = rel.strip_suffix("/index") {
            format!("/{}", stripped)
        } else {
            format!("/{}", rel)
        };

        let content = fs::read_to_string(&path).unwrap_or_default();
        let guard = crate::auth::extract_page_guard(&content);
        
        // Extract cache prop using the same layout regex and props parser
        let cache = LAYOUT_TAG_RE.captures(&content)
            .and_then(|c| parse_props_v7(c.get(2)?.as_str()).get("cache").cloned())
            .and_then(|v| v.as_str().map(String::from));

        // Pass `rel` (file path) to renderer — layout resolution walks the path
        // hierarchy (admin/index → tries layouts/admin/indexLayout then layouts/adminLayout)
        let rendered = crate::router::render_vlo_for_build_at(&rel, content);
        // Pass `route` (URL path) to wrap_html — better title fallback than "admin/index"
        let html = crate::router::wrap_html(&route, &rendered, true);

        // Output path mirrors source structure: admin/index.vlo → admin/index.html
        let out = if rel == "index" {
            build_dir.join("index.html")
        } else {
            build_dir.join(format!("{}.html", rel))
        };
        
        if let Some(p) = out.parent() { 
            fs::create_dir_all(p).map_err(|e| format!("Dir error: {}", e))?; 
        }
        fs::write(&out, html).map_err(|e| format!("Write error: {}", e))?;

        let mut cfg = serde_json::Map::new();
        if let Some(g) = guard { 
            cfg.extend([
                ("auth".into(), g.auth.into()), 
                ("guest".into(), g.guest.into()), 
                ("roles".into(), g.roles.into())
            ]); 
        }
        if let Some(c) = cache { cfg.insert("cache".into(), c.into()); }
        if !cfg.is_empty() { manifest.insert(route, cfg.into()); }

        println!("  ├─ Generated: {}", out.display());
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("copy {} -> {}: {}", src_path.display(), dst_path.display(), e),
                )
            })?;
        }
    }
    Ok(())
}

pub async fn deploy(provider: &str) -> Result<(), String> {
    let root = get_project_root();
    let build_dir = root.join(".vlo").join("build");

    if !build_dir.exists() {
        println!("⚡ Production build not found. Running build...");
        build(true)?;
    }
    if !build_dir.exists() {
        return Err("Production build failed.".to_string());
    }

    let provider = provider.to_lowercase();
    println!("⚡ Deploying /.vlo/build to {}...", provider);

    if provider == "railway" {
        let caddy = build_dir.join("Caddyfile");
        if !caddy.exists() {
            fs::write(
                &caddy,
                ":$PORT {\n    root * .\n    file_server\n}\n",
            )
            .map_err(|e| format!("Failed to write Caddyfile: {}", e))?;
        }
    }

    let args: Vec<&str> = match provider.as_str() {
        "netlify" => vec!["netlify-cli", "deploy", "--dir=.vlo/build", "--prod"],
        "vercel" => vec!["vercel", "deploy", ".vlo/build", "--prod"],
        "cloudflare" | "pages" => vec!["wrangler", "pages", "deploy", ".vlo/build"],
        "railway" => vec!["@railway/cli", "up"],
        _ => return Err(format!("Unsupported provider '{}'.", provider)),
    };

    let working_dir = if provider == "railway" { &build_dir } else { &root };

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
    }
    .map_err(|e| format!("Failed to execute deploy command: {}", e))?;

    if !status.success() {
        return Err(format!("Deployment exited with status: {}", status));
    }

    println!("⚡ Deployment completed successfully!");
    Ok(())
}

// ─────────────────────────────────────────────────────────────
// Directive resolution (single pass)
// ─────────────────────────────────────────────────────────────
static RE_ALL_DIRECTIVES: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?is)<([a-zA-Z][a-zA-Z0-9-]*)([^>]*?)\s+v-(delete|put|post)\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    )
    .unwrap()
});

static RE_STRIP: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?is)\s+v-(?:put|delete|post|prompt|param|confirm)\s*=\s*["'][^"']*["']"#,
    )
    .unwrap()
});

static RE_STRIP_AUTO: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)\s+v-auto(?:\s*=\s*["'][^"']*["'])?(\s|$)"#).unwrap()
});

static RE_HAS_AUTO: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)(?:^|\s)v-auto(?:\s|=|$)"#).unwrap()
});

pub fn strip_vlo_directive_attrs(attrs: &str) -> String {
    let stripped = RE_STRIP.replace_all(attrs, "");
    RE_STRIP_AUTO.replace_all(&stripped, "$1").into_owned()
}

/// Word-boundary aware attribute lookup. Avoids `v-param` matching
/// `data-v-param` when searching.
fn attr_val<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let idx = attrs.find(name)?;
    let before_ok = idx == 0 || !attrs.as_bytes()[idx - 1].is_ascii_alphanumeric();
    let after = attrs.as_bytes().get(idx + name.len()).copied();
    let after_ok = !matches!(after, Some(b) if b.is_ascii_alphanumeric() || b == b'-');
    if !before_ok || !after_ok {
        return None;
    }

    let rest = attrs[idx + name.len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &rest[1..];
    rest.find(quote).map(|e| &rest[..e])
}

pub fn resolve_directives(source: &str) -> String {
    let result = RE_ALL_DIRECTIVES.replace_all(source, |caps: &regex::Captures| {
        let tag = caps.get(1).unwrap().as_str();
        let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let kind = caps.get(3).unwrap().as_str();
        let url_raw = caps.get(4).map(|m| m.as_str()).unwrap_or("");
        let attrs_after = caps.get(5).map(|m| m.as_str()).unwrap_or("");

        let url = url_raw.replace("|ajax", "");
        let all_attrs = format!("{} {}", attrs_before, attrs_after);

        let clean_before = strip_vlo_directive_attrs(attrs_before);
        let clean_after = strip_vlo_directive_attrs(attrs_after);
        let all_clean = format!("{} {}", clean_before.trim(), clean_after.trim())
            .trim()
            .to_string();

        let data_attrs = match kind {
            "delete" => {
                let mut s = format!("data-vlo-delete=\"{}\"", escape_html_attribute(&url));
                let confirm = attr_val(&all_attrs, "v-confirm").unwrap_or("");
                if !confirm.is_empty() {
                    s.push_str(&format!(" data-confirm=\"{}\"", escape_html_attribute(confirm)));
                }
                s
            }
            "put" => {
                let mut s = format!("data-vlo-put=\"{}\"", escape_html_attribute(&url));
                if tag.eq_ignore_ascii_case("form") {
                    if RE_HAS_AUTO.is_match(&all_attrs) {
                        s.push_str(" data-vlo-auto");
                    }
                } else {
                    let param = attr_val(&all_attrs, "v-param").unwrap_or("value");
                    let prompt = attr_val(&all_attrs, "v-prompt").unwrap_or("Enter new value:");
                    s.push_str(&format!(" data-vlo-param=\"{}\"", escape_html_attribute(param)));
                    s.push_str(&format!(" data-vlo-prompt=\"{}\"", escape_html_attribute(prompt)));
                }
                s
            }
            "post" => format!("data-vlo-post=\"{}\"", escape_html_attribute(&url)),
            _ => unreachable!(),
        };

        format!("<{} {} {}>", tag, all_clean, data_attrs)
    });

    vlo_debug!(
        "🧩 [VLO DIRECTIVES] Resolved HTML: {} chars, {} rows",
        result.len(),
        result.lines().count()
    );
    result.into_owned()
}

// ─────────────────────────────────────────────────────────────
// Conditional script injection
// ─────────────────────────────────────────────────────────────
pub fn inject_conditional_scripts(source: &str) -> String {
    let has_delete = source.contains("data-vlo-delete");
    let has_put = source.contains("data-vlo-put");
    let has_post = source.contains("data-vlo-post");
    let has_modal = source.contains("data-modal-open") || source.contains("data-modal-close");

    if !(has_delete || has_put || has_post || has_modal) {
        return source.to_string();
    }

    let mut scripts: Vec<&str> = Vec::with_capacity(5);
    if has_delete || has_put || has_post {
        scripts.push(VLO_SHARED_RESPONSE_HANDLER_JS);
    }
    if has_delete {
        scripts.push(VLO_DELETE_HANDLER_JS);
    }
    if has_put {
        scripts.push(VLO_PUT_HANDLER_JS);
    }
    if has_post {
        scripts.push(VLO_POST_HANDLER_JS);
    }
    if has_modal {
        scripts.push(VLO_MODAL_HANDLER_JS);
    }

    let combined = scripts.join("\n");
    let block = format!(
        "<script id=\"__VLO_DIRECTIVE_HANDLERS__\">\n{}\n</script>",
        combined
    );

    match source.rfind("</body>") {
        Some(pos) => {
            let mut out = String::with_capacity(source.len() + block.len());
            out.push_str(&source[..pos]);
            out.push_str(&block);
            out.push_str(&source[pos..]);
            out
        }
        None => format!("{}\n{}", source, block),
    }
}

// ─────────────────────────────────────────────────────────────
// JS constants (unchanged — full feature coverage)
// ─────────────────────────────────────────────────────────────
const VLO_SHARED_RESPONSE_HANDLER_JS: &str = r##"
function vloParseAlert(f){if(!f)return null;var v=f.getAttribute('data-vlo-alert')||f.getAttribute('alert');if(v===null||v==='')return null;if(v==='true'||v==='1')return{tag:'div',selector:null};if(v==='false'||v==='0')return null;var c=v.indexOf(':');if(c===-1)return{tag:'div',selector:v.trim()};var t=v.slice(0,c).trim().toLowerCase()||'div';var s=v.slice(c+1).trim();return{tag:t,selector:s||null}}

function vloEsc(v){return String(v==null?'':v).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;')}

function vloInsertAlert(f,cfg,cls,css,html){if(!cfg||!f)return;var el=document.createElement(cfg.tag);el.className=cls;el.style.cssText=css;el.innerHTML=html;if(cfg.selector){var t=document.querySelector(cfg.selector);if(t){t.appendChild(el);return}}var b=f.querySelector('button[type="submit"]');if(b)b.parentNode.insertBefore(el,b);else f.appendChild(el)}

function vloShouldReload(f){return f&&!f.hasAttribute('data-vlo-no-reload')&&!f.hasAttribute('data-vlo-auto')}

function vloSafeRedirect(v){if(!v)return null;if(typeof v==='object'&&v.redirect)return vloSafeRedirect(v.redirect);if(typeof v==='string'){var s=v.trim();if(s.charAt(0)==='{'){try{var o=JSON.parse(s);if(o&&o.redirect)return vloSafeRedirect(o.redirect)}catch(_){}}if(s.charAt(0)==='/'&&s.charAt(1)!=='/'&&s.indexOf('://')===-1)return s}return null}

function vloHandleResponse(r,f){
  return r.json().then(function(d){
    var ac=vloParseAlert(f);
    if(!r.ok){
      if(d.activation_required){
        if(ac&&f){var o=f.querySelector('.vlo-inline-error');if(o)o.remove();vloInsertAlert(f,ac,'auth-alert vlo-inline-error','display:flex;flex-direction:column;gap:10px;margin-bottom:15px;padding:16px;background:rgba(0,245,255,0.05);border:1px solid rgba(0,245,255,0.25);border-radius:8px;font-size:0.9rem;','<div style="display:flex;align-items:center;gap:10px"><span style="background:rgba(0,245,255,0.1);color:#00f5ff;font-weight:bold;font-size:1.2rem;width:24px;height:24px;display:grid;place-items:center;border-radius:7px">i</span><p style="margin:0;color:#80f0ff">'+vloEsc(d.error||'Account is not active')+' Please check your email.</p></div><button type="button" onclick="openResendActivationModal()" style="margin-top:8px;padding:8px 12px;background:#00f5ff;color:#000;border:none;border-radius:6px;font-weight:700;cursor:pointer;font-size:0.8rem;">Resend Activation Email</button>')}
        throw new Error("activation_required")
      }
      if(d.warning||(r.ok&&d.success&&d.warning)){
        if(ac&&f){var o=f.querySelector('.vlo-inline-error');if(o)o.remove();vloInsertAlert(f,ac,'auth-alert vlo-inline-error','display:flex;align-items:center;gap:10px;margin-bottom:15px;padding:12px;background:rgba(255,170,0,0.1);border:1px solid rgba(255,170,0,0.3);border-radius:8px;font-size:0.9rem;','<span style="font-weight:bold;font-size:1.2rem">!</span><p style="margin:0;color:#ffaa00">'+vloEsc(d.message||'Warning')+'</p>')}
        if(f)setTimeout(function(){var m=f.closest('.vlo-modal-overlay');if(m)m.classList.remove('active');else if(vloShouldReload(f))window.location.reload()},4000);
        throw new Error("warning")
      }
      throw new Error(d.error||d.details||d.message||'Request failed')
    }
    var u=vloSafeRedirect(d.redirect)||(Array.isArray(d.data)&&d.data[0]&&vloSafeRedirect(d.data[0].redirect));
    if(u){window.location.href=u;return}
    if(f){
      var o=f.querySelector('.vlo-inline-error');if(o)o.remove();
      if(d.message&&ac)vloInsertAlert(f,ac,'auth-alert vlo-inline-error','display:flex;align-items:center;gap:10px;color:#00ff88;margin-bottom:15px;padding:12px;background:rgba(0,255,136,0.1);border:1px solid rgba(0,255,136,0.3);border-radius:8px;font-size:0.9rem;','<span style="font-weight:bold;font-size:1.2rem">OK</span><p style="margin:0;color:#00ff88">'+vloEsc(d.message)+'</p>')
      if(!f.hasAttribute('data-vlo-auto'))f.reset();
      setTimeout(function(){var m=f.closest('.vlo-modal-overlay');if(m)m.classList.remove('active');else if(vloShouldReload(f))window.location.reload()},2000)
    }
    window.dispatchEvent(new CustomEvent('vlo:mutation'));
    return d
  })
}

function vloShowError(e,f){
  if(e.message==="activation_required"||e.message==="warning")return;
  console.error('[VLO API]',e);
  if(!f){
    var t=document.createElement('div');t.className='vlo-flash';t.style.cssText='position:fixed;top:20px;right:20px;z-index:99999;padding:16px 22px;border-radius:10px;background:#1a1a2e;border-left:4px solid #ff4444;color:#fff;box-shadow:0 8px 24px rgba(0,0,0,0.4);display:flex;align-items:center;gap:12px;max-width:380px;animation:vloFlashIn 0.35s ease';
    t.innerHTML='<span style="font-size:1.4rem">!</span><div><strong style="display:block;font-size:0.9rem;margin-bottom:2px">Error</strong><span style="color:#aaa;font-size:0.78rem">'+vloEsc(e.message||'Request failed')+'</span></div>';
    document.body.appendChild(t);
    setTimeout(function(){t.style.transition='opacity 0.4s';t.style.opacity='0';setTimeout(function(){t.remove()},400)},4000);
    return
  }
  var o=f.querySelector('.vlo-inline-error');if(o)o.remove();
  var err=document.createElement('div');err.className='auth-alert vlo-inline-error';
  err.style.cssText='display:flex;align-items:center;gap:10px;color:#ff4444;margin-bottom:15px;padding:12px;background:rgba(255,68,68,0.1);border:1px solid rgba(255,68,68,0.3);border-radius:8px;font-size:0.9rem;';
  err.innerHTML='<span style="font-weight:bold;font-size:1.2rem">!</span><p style="margin:0;color:#ff4444">'+vloEsc(e.message||'Request failed')+'</p>';
  var b=f.querySelector('button[type="submit"]');
  if(b)b.parentNode.insertBefore(err,b);else f.appendChild(err)
}
"##;

const VLO_DELETE_HANDLER_JS: &str = r#"
document.addEventListener('click', function(e) {
    const btn = e.target.closest('[data-vlo-delete]');
    if (!btn) return;
    e.preventDefault();
    const url = btn.getAttribute('data-vlo-delete');
    const confirmMsg = btn.getAttribute('data-confirm');
    if (confirmMsg && !confirm(confirmMsg)) return;
    const origText = btn.innerText;
    btn.innerText = 'Deleting...';
    btn.disabled = true;
    fetch(url, { credentials: 'same-origin', method: 'DELETE', headers: { 'X-CSRF-Token': document.querySelector('meta[name="csrf-token"]')?.content || '' } })
    .then(async function(r) {
        var d; try { d = await r.json() } catch(x) { d = {} }
        if (!r.ok) throw new Error(d.error || d.details || d.message || 'Request failed');
        window.dispatchEvent(new CustomEvent('vlo:mutation'));
    })
    .catch(function(e) {
        console.error('[VLO DELETE]', e);
        var t = document.createElement('div');
        t.className = 'vlo-flash';
        t.style.cssText = 'position:fixed;top:20px;right:20px;z-index:99999;padding:16px 22px;border-radius:10px;background:#1a1a2e;border-left:4px solid #ff4444;color:#fff;box-shadow:0 8px 24px rgba(0,0,0,0.4);display:flex;align-items:center;gap:12px;max-width:380px;animation:vloFlashIn 0.35s ease';
        t.innerHTML = '<span style="font-size:1.4rem">⚠️</span><div><strong style="display:block;font-size:0.9rem;margin-bottom:2px">Error</strong><span style="color:#aaa;font-size:0.78rem">' + vloEsc(e.message || 'Request failed') + '</span></div>';
        document.body.appendChild(t);
        setTimeout(function() { t.style.transition = 'opacity 0.4s'; t.style.opacity = '0'; setTimeout(function() { t.remove(); }, 400); }, 4000);
    })
    .finally(function() { btn.innerText = origText; btn.disabled = false; });
});
"#;

const VLO_POST_HANDLER_JS: &str = r#"
document.addEventListener('submit', function(e) {
    const form = e.target;
    const url = form.getAttribute('data-vlo-post');
    if (!url) return;
    e.preventDefault();
    if (!form.checkValidity()) { form.reportValidity(); return; }
    const submitBtn = form.querySelector('button[type="submit"]');
    if (submitBtn) { submitBtn.disabled = true; submitBtn.dataset.origText = submitBtn.innerText; submitBtn.innerText = 'Processing...'; }
    const mp = form.enctype === 'multipart/form-data';
    const h = { 'X-CSRF-Token': document.querySelector('meta[name="csrf-token"]')?.content || '' };
    if (!mp) h['Content-Type'] = 'application/x-www-form-urlencoded';
    const bd = mp ? new FormData(form) : new URLSearchParams(new FormData(form));
    const un = new URLSearchParams(window.location.search).get('next');
    if (un && !bd.has('next')) bd.append('next', un);
    fetch(url, { credentials: 'same-origin', method: 'POST', headers: h, body: bd })
        .then(function(r) { return vloHandleResponse(r, form); })
        .catch(function(e) { vloShowError(e, form); })
        .finally(function() { if (submitBtn) { submitBtn.disabled = false; submitBtn.innerText = submitBtn.dataset.origText || 'Submit'; } });
});
"#;

const VLO_PUT_HANDLER_JS: &str = r#"
document.addEventListener('change', function(e) {
    var form = e.target && e.target.closest ? e.target.closest('form[data-vlo-put][data-vlo-auto]') : null;
    if (!form) return;
    clearTimeout(form.__vloAutoTimer);
    form.__vloAutoTimer = setTimeout(function() {
        if (!form.isConnected) return;
        if (form.requestSubmit) form.requestSubmit();
        else form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
    }, 350);
});

document.addEventListener('submit', function(e) {
    const form = e.target;
    const url = form.getAttribute('data-vlo-put');
    if (!url) return;
    e.preventDefault();
    if (form.__vloBusy) { form.__vloPending = true; return; }
    if (!form.checkValidity()) { form.reportValidity(); return; }
    form.__vloBusy = true;
    form.__vloPending = false;
    if (form.hasAttribute('data-vlo-auto')) form.setAttribute('aria-busy', 'true');
    const submitBtn = form.querySelector('button[type="submit"]');
    if (submitBtn) { submitBtn.disabled = true; submitBtn.dataset.origText = submitBtn.innerText; submitBtn.innerText = 'Saving...'; }
    const mp = form.enctype === 'multipart/form-data';
    const h = { 'X-CSRF-Token': document.querySelector('meta[name="csrf-token"]')?.content || '' };
    if (!mp) h['Content-Type'] = 'application/x-www-form-urlencoded';
    const bd = mp ? new FormData(form) : new URLSearchParams(new FormData(form));
    fetch(url, { credentials: 'same-origin', method: 'PUT', headers: h, body: bd })
        .then(function(r) { return vloHandleResponse(r, form); })
        .catch(function(e) { vloShowError(e, form); })
        .finally(function() {
            form.__vloBusy = false;
            form.removeAttribute('aria-busy');
            if (submitBtn) { submitBtn.disabled = false; submitBtn.innerText = submitBtn.dataset.origText || 'Save'; }
            if (form.__vloPending && form.isConnected) {
                form.__vloPending = false;
                if (form.requestSubmit) form.requestSubmit();
                else form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
            }
        });
});

document.addEventListener('click', function(e) {
    const btn = e.target.closest('[data-vlo-put]');
    if (!btn || btn.tagName.toLowerCase() === 'form') return;
    e.preventDefault();
    const url = btn.getAttribute('data-vlo-put');
    const param = btn.getAttribute('data-vlo-param') || 'value';
    const promptMsg = btn.getAttribute('data-vlo-prompt') || 'Enter new value:';
    const v = prompt(promptMsg);
    if (v === null) return;
    const origText = btn.innerText;
    btn.innerText = 'Saving...';
    btn.disabled = true;
    fetch(url, { credentials: 'same-origin', method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': document.querySelector('meta[name="csrf-token"]')?.content || '' }, body: JSON.stringify({ [param]: v }) })
    .then(async function(r) {
        var d; try { d = await r.json() } catch(x) { d = {} }
        if (!r.ok) throw new Error(d.error || d.details || d.message || 'Request failed');
        window.dispatchEvent(new CustomEvent('vlo:mutation'));
    })
    .catch(function(e) {
        console.error('[VLO PUT]', e);
        var t = document.createElement('div');
        t.className = 'vlo-flash';
        t.style.cssText = 'position:fixed;top:20px;right:20px;z-index:99999;padding:16px 22px;border-radius:10px;background:#1a1a2e;border-left:4px solid #ff4444;color:#fff;box-shadow:0 8px 24px rgba(0,0,0,0.4);display:flex;align-items:center;gap:12px;max-width:380px;animation:vloFlashIn 0.35s ease';
        t.innerHTML = '<span style="font-size:1.4rem">⚠️</span><div><strong style="display:block;font-size:0.9rem;margin-bottom:2px">Error</strong><span style="color:#aaa;font-size:0.78rem">' + vloEsc(e.message || 'Request failed') + '</span></div>';
        document.body.appendChild(t);
        setTimeout(function() { t.style.transition = 'opacity 0.4s'; t.style.opacity = '0'; setTimeout(function() { t.remove(); }, 400); }, 4000);
    })
    .finally(function() { btn.innerText = origText; btn.disabled = false; });
});
"#;

const VLO_MODAL_HANDLER_JS: &str = r#"
document.addEventListener('click', function(e) {
    function findModal(trigger, id) {
        if (id) {
            var modal = document.getElementById(id);
            if (modal) return modal;
            modal = document.querySelector('.' + id);
            if (modal) return modal;
        }
        return trigger.closest('.vlo-modal-overlay');
    }
    var openBtn = e.target.closest('[data-modal-open]');
    if (openBtn) {
        e.preventDefault();
        var modal = findModal(openBtn, openBtn.getAttribute('data-modal-open'));
        if (modal) modal.classList.add('active');
        return;
    }
    var closeBtn = e.target.closest('[data-modal-close]');
    if (closeBtn) {
        e.preventDefault();
        var modal = findModal(closeBtn, closeBtn.getAttribute('data-modal-close'));
        if (modal) modal.classList.remove('active');
        return;
    }
    var overlay = e.target.closest('.vlo-modal-overlay');
    if (overlay && e.target === overlay) overlay.classList.remove('active');
});

document.addEventListener('keydown', function(e) {
    if (e.key === 'Escape') {
        var active = document.querySelector('.vlo-modal-overlay.active');
        if (active) active.classList.remove('active');
    }
});
"#;

// SSE + Broadcast runtime — injected only when a page uses vlo-sync / data-channel
const SSE_SCRIPT: &str = r#"<script id="__VLO_SSE__">
(function(){
  window.__VLO_BROADCAST__ = function(channel, value) {
    var csrfToken = '__CSRF_TOKEN__';
    if (csrfToken === '__VLO_CSRF_PLACEHOLDER__') {
      csrfToken = document.querySelector('meta[name="csrf-token"]').content || '';
    }
    fetch('/api/broadcast', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': csrfToken },
      credentials: 'same-origin',
      body: JSON.stringify({ channel: channel, value: value })
    }).catch(function(e) { console.error('[VLO Broadcast]', e); });
  };
  window.__VLO_SSE__ = new EventSource("/__vlo_sse");
  window.__VLO_SSE__.onmessage = function(e) {
    try {
      var data = JSON.parse(e.data);
      var els = document.querySelectorAll('[data-channel="' + data.channel + '"]');
      for (var i = 0; i < els.length; i++) {
        if (typeof data.value === 'object' && data.value !== null) {
          window.dispatchEvent(new CustomEvent('vlo:mutation'));
        } else {
          els[i].textContent = data.value;
          els[i].classList.add('vlo-sync-updated');
          (function(el){ setTimeout(function(){ el.classList.remove('vlo-sync-updated'); }, 600); })(els[i]);
        }
      }
    } catch(err) { console.error('VLO SSE Error:', err); }
  };
  window.__VLO_SSE__.onerror = function(e) { console.error('VLO SSE connection error', e); };
})();
</script>"#;

#[cfg(test)]
mod directive_tests {
    use super::*;

    #[test]
    fn v_auto_on_put_form_compiles_and_is_stripped() {
        let out = resolve_directives(
            r#"<form v-put="/api/cart_items/7" v-auto class="cart-item-actions" alert="false"><input name="quantity" value="1"></form>"#,
        );
        assert!(out.contains(r#"data-vlo-put="/api/cart_items/7""#), "{out}");
        assert!(out.contains("data-vlo-auto"), "{out}");
        assert!(!out.contains("v-auto"), "{out}");
        assert!(out.contains(r#"class="cart-item-actions""#), "{out}");
        assert!(out.contains(r#"alert="false""#), "{out}");
    }

    #[test]
    fn v_auto_accepts_explicit_value() {
        let out = resolve_directives(r#"<form v-auto="true" v-put="/api/x">"#);
        assert!(out.contains("data-vlo-auto"), "{out}");
        assert!(!out.contains("v-auto"), "{out}");
    }

    #[test]
    fn v_auto_ignored_outside_put_forms() {
        let out = resolve_directives(r#"<button v-put="/api/x" v-auto>Save</button>"#);
        assert!(out.contains(r#"data-vlo-put="/api/x""#), "{out}");
        assert!(!out.contains("data-vlo-auto"), "{out}");
        assert!(!out.contains("v-auto"), "{out}");
    }

    #[test]
    fn plain_put_form_has_no_auto_flag() {
        let out = resolve_directives(r#"<form v-put="/api/x" class="f">"#);
        assert!(out.contains(r#"data-vlo-put="/api/x""#), "{out}");
        assert!(!out.contains("data-vlo-auto"), "{out}");
    }

    #[test]
    fn v_auto_prefix_is_not_overmatched() {
        let out = resolve_directives(r#"<form v-put="/api/x" data-v-autofocus="1">"#);
        assert!(!out.contains("data-vlo-auto"), "{out}");
        assert!(out.contains(r#"data-v-autofocus="1""#), "{out}");
    }

    #[test]
    fn strip_keeps_neighbouring_attributes() {
        assert_eq!(strip_vlo_directive_attrs(" v-auto class=\"a\""), " class=\"a\"");
        assert_eq!(strip_vlo_directive_attrs(" v-auto"), "");
    }
}
