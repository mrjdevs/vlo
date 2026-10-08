use crate::{
    api::{api_handler_id, api_handler_path, api_handler_root},
    files_api::{delete_file, download_file, get_file, serve_file, upload_file},
    router::{hmr_handler, home_handler, not_found_handler, page_handler, watch_files},
    state::{self, get_project_root},
    template::escape_html_attribute,
};
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use crate::auth;
use axum::extract::DefaultBodyLimit;
use clap::Subcommand;
use std::{
    collections::HashMap,
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex, LazyLock, OnceLock},
    time::Instant,
};
use tokio::sync::broadcast;
use tower_http::{compression::CompressionLayer, services::ServeDir};
use axum::middleware::{self, Next};
use axum::extract::Request;
use axum::response::Response;
use axum::http::header::{CACHE_CONTROL, HeaderValue};


// ─── PRODUCTION CACHE: Eliminates disk I/O on every request ───
struct BuildCache {
    manifest: serde_json::Value,
    pages: HashMap<String, Arc<str>>,
}

static BUILD_CACHE: OnceLock<BuildCache> = OnceLock::new();

fn ensure_build_cache(build_dir: &Path) {
    BUILD_CACHE.get_or_init(|| {
        let manifest_path = build_dir.join("routes.json");
        let manifest = if manifest_path.exists() {
            std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
        } else {
            serde_json::Value::Object(serde_json::Map::new())
        };

        let mut pages = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(build_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("html") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let name = path.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("index")
                            .to_string();
                        let name = if name == "index" { "/".to_string() } else { format!("/{}", name) };
                        pages.insert(name, Arc::from(content));
                    }
                }
            }
        }
        
        BuildCache { manifest, pages }
    });
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(name = "init", alias = "new")]
    Init {
        #[arg(default_value = ".")]
        name: String,
        #[arg(
            short,
            long,
            default_value = "sqlite",
            value_parser = ["sqlite", "postgres", "mysql"]
        )]
        db: String,
        #[arg(long = "db-name", alias = "db_name")]
        db_name: Option<String>,
        #[arg(long)]
        no_db: bool,
    },

    Dev {
        #[arg(short, long)]
        port: Option<String>,  // <--- Clap parses this as u16 automatically
        #[arg(long)]
        host: Option<String>,
    },

    Build {
        #[arg(long)]
        release: bool,
    },

    Serve {
        #[arg(short, long)]
        port: Option<String>,
        #[arg(long)]
        host: Option<String>,
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
// ---------------------------------------------------------------------------
// Health Check Endpoint
// ---------------------------------------------------------------------------
async fn healthz_handler() -> impl IntoResponse {
    let db_status = crate::database::DB_POOL.get().is_some();
    let status = if db_status { "healthy" } else { "degraded" };
    let http_status = if db_status { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };

    (http_status, axum::Json(serde_json::json!({
        "status": status,
        "uptime_seconds": crate::state::uptime_seconds(),
        "database": if db_status { "connected" } else { "disconnected" },
        "version": env!("CARGO_PKG_VERSION")
    })))
}

// ---------------------------------------------------------------------------
// Request ID Middleware
// ---------------------------------------------------------------------------
async fn request_id_middleware(mut req: Request, next: Next) -> Response {
    let request_id = req.headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| crate::state::generate_request_id());
    
    req.extensions_mut().insert(crate::state::RequestId(request_id.clone()));
    
    let mut response = next.run(req).await;
    response.headers_mut().insert(
        "x-request-id",
        request_id.parse().unwrap(),
    );
    response
}
// ---------------------------------------------------------------------------
// Background: Rate Limit Cleanup Task
// ---------------------------------------------------------------------------
async fn rate_limit_cleanup_task() {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(300)).await; // 5 minutes
        crate::auth::cleanup_rate_limits();
        crate::vlo_debug!("🧹 Rate limit cleanup completed");
    }
}

async fn session_cleanup_task() {
    // Run immediately on startup to clear stale sessions from when server was offline
    match crate::auth::cleanup_expired_sessions().await {
        Ok(count) => {
            if count > 0 {
                crate::vlo_debug!("🧹 Startup session cleanup: purged {} expired sessions", count);
            }
        }
        Err(e) => crate::vlo_debug!("⚠️ Startup session cleanup failed: {}", e),
    }

    loop {
        // Sleep for 1 hour (3600 seconds)
        tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        match crate::auth::cleanup_expired_sessions().await {
            Ok(count) => {
                if count > 0 {
                    crate::vlo_debug!("🧹 Hourly session cleanup: purged {} expired sessions", count);
                }
            }
            Err(e) => crate::vlo_debug!("⚠️ Hourly session cleanup failed: {}", e),
        }
    }
}

// ─── HELPER 1: Common Routes (No layers yet) ─────────────────────────
fn build_common_routes() -> Router {
    Router::new()
        .route("/uploads/*path", get(serve_file))
        .route("/healthz", get(healthz_handler))
        .route("/api/files/upload", axum::routing::post(upload_file))
        .route("/api/files/:id/download", get(download_file))
        .route("/api/files/:id", get(get_file).delete(delete_file))
        
        // 🔥 CRITICAL: Specific routes MUST be defined BEFORE generic catch-alls
        .route("/api/auth/login", axum::routing::post(auth::login_handler))
        .route("/api/auth/logout", axum::routing::post(auth::logout_handler).get(auth::logout_handler))
        .route("/api/auth/me", axum::routing::get(auth::me_handler))
        .route("/api/auth/register", axum::routing::post(auth::register_handler))
        .route("/api/auth/activate", axum::routing::get(auth::activate_handler))
        .route("/api/auth/resend-activation", axum::routing::post(auth::resend_activation_handler))
        .route("/api/auth/forgot-password", axum::routing::post(auth::forgot_password_handler))
        .route("/api/auth/reset-password", axum::routing::post(auth::reset_password_handler))

        // Generic API routes (Catch-alls) come LAST
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
        .route("/__vlo_sse", get(crate::router::sse_handler))
        .route("/__vlo/ajax.js", get(crate::router::ajax_js_handler))
        .route("/api/broadcast", axum::routing::post(broadcast_handler))
}

// ─── HELPER 2: Apply Common Middleware Layers ────────────────────────
fn apply_common_layers(app: Router) -> Router {
    app.layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn(cache_middleware))
        .layer(middleware::from_fn(request_id_middleware))
        .layer(axum::middleware::from_fn(auth::csrf_middleware))
        .layer(axum::middleware::from_fn(auth::api_auth_middleware))
        .layer(axum::middleware::from_fn(auth::session_middleware))
}

// ─── HELPER 3: Common Server Startup Logic ───────────────────────────
async fn run_server(
    mode: state::AppMode,
    app: Router,
    host: Option<&str>,
    port: Option<u16>,
    server_type: &str,
) -> Result<(), String> {
    state::set_app_mode(mode);
    crate::router::init_live_broadcast();

    let host_str = host
        .map(str::to_string)
        .or_else(|| std::env::var("VLO_HOST").ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let port_num = port
        .or_else(|| {
            std::env::var("VLO_PORT")
                .ok()
                .and_then(|value| value.parse::<u16>().ok())
        })
        .unwrap_or(3000);

    let addr = format!("{}:{}", host_str, port_num);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(error) => {
            return Err(format!(
                "Failed to start VLO {} server.\n   Address: {}\n   Error: {}\n   Try another port with: vlo {} --port 3001",
                server_type, addr, error, server_type
            ));
        }
    };

    println!("⚡ VLO {} server: http://{}", server_type, addr);
    
    // Spawn background cleanup tasks
    tokio::spawn(rate_limit_cleanup_task());
    tokio::spawn(session_cleanup_task());
    
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        return Err(format!(
            "VLO {} server stopped unexpectedly.\n   Error: {}",
            server_type, error
        ));
    }

    Ok(())
}

// ─── DEV SERVER ──────────────────────────────────────────────────────
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
        .fallback(not_found_handler);

    let app = apply_common_layers(app);

    run_server(state::AppMode::Development, app, host, port, "dev").await
}

// ─── PRODUCTION SERVER ───────────────────────────────────────────────
pub async fn serve(host: Option<&str>, port: Option<u16>) -> Result<(), String> {
    let root = get_project_root();
    let build_dir = root.join(".vlo").join("build");
    let static_dir = build_dir.join("static");

    if !build_dir.exists() {
        return Err("Production build not found.\n   Run `vlo build` first.".to_string());
    }

    let app = build_common_routes()
        .nest_service("/static", ServeDir::new(static_dir))
        .fallback(move |uri: axum::http::Uri, req: Request| async move {
            serve_build_page(build_dir.clone(), uri, req).await
        });

    let app = apply_common_layers(app);

    run_server(state::AppMode::Production, app, host, port, "production").await
}

pub async fn cgi() -> Result<(), String> {
    let root = crate::state::get_project_root();

    let request_uri = std::env::var("REQUEST_URI")
        .unwrap_or_else(|_| "/".to_string());

    let path = request_uri
        .split('?')
        .next()
        .unwrap_or("/")
        .trim_matches('/');

    if path.ends_with(".vlo") || path.starts_with("pages/") {
        println!("Status: 404 Not Found");
        println!("Content-Type: text/html; charset=utf-8");
        println!();
        println!("<h1>404</h1><p>Page Not Found</p>");
        return Ok(());
    }

    let page_name = if path.is_empty() {
        "home".to_string()
    } else {
        path.to_string()
    };

    if page_name.contains("..") || page_name.contains('\\') {
        println!("Status: 404 Not Found");
        println!("Content-Type: text/html; charset=utf-8");
        println!();
        println!("<h1>404</h1><p>Page Not Found</p>");
        return Ok(());
    }

    let page_file = root
        .join("pages")
        .join(format!("{}.vlo", page_name));

    let query_string = std::env::var("QUERY_STRING")
        .unwrap_or_default();

    let query = parse_cgi_query(&query_string);

    match fs::read_to_string(&page_file) {
        Ok(source) => {
            let rendered = crate::router::render_vlo_with_query(
                source,
                &query,
            );

            let html = crate::router::wrap_html(
                &page_name,
                &rendered,
                true,
            );

            println!("Status: 200 OK");
            println!("Content-Type: text/html; charset=utf-8");
            println!();
            print!("{}", html);
        }

        Err(_) => {
            let response = crate::router::render_404()
                .await
                .into_response();

            let status = response.status();

            println!(
                "Status: {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Not Found")
            );
            println!("Content-Type: text/html; charset=utf-8");
            println!();

            let body = axum::body::to_bytes(
                response.into_body(),
                usize::MAX,
            )
            .await
            .map_err(|error| {
                format!("Failed to read CGI response: {}", error)
            })?;

            print!("{}", String::from_utf8_lossy(&body));
        }
    }

    Ok(())
}

fn parse_cgi_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let mut parts = part.splitn(2, '=');
            let key = parts.next()?.to_string();
            let value = parts.next().unwrap_or("").to_string();
            Some((key, value))
        })
        .collect()
}

async fn serve_build_page(
    build_dir: std::path::PathBuf,
    uri: axum::http::Uri,
    req: Request,
) -> impl IntoResponse {
    // 🔥 FIX: Load manifest and pages into memory ONCE. 
    // Subsequent requests have ZERO disk I/O for file reads.
    ensure_build_cache(&build_dir);
    let cache = BUILD_CACHE.get().unwrap();

    let path = uri.path();

    // ─── 1. RESOLVE AUTH ───────────────────────────────────
    let mut auth = req.extensions().get::<crate::auth::AuthUser>().cloned();
    if auth.is_none() || auth.as_ref().map_or(true, |a| a.user.is_none()) {
        let cookie_header = req.headers().get(axum::http::header::COOKIE).and_then(|v| v.to_str().ok());
        let cookie_name = crate::auth::auth_config().cookie_name.clone();
        if let Some(header) = cookie_header {
            if let Some(token) = crate::auth::parse_cookie_header(header, &cookie_name) {
                let user = crate::auth::get_user_from_session(&token).await;
                if user.is_some() {
                    auth = Some(crate::auth::AuthUser {
                        user,
                        csrf_token: Some(crate::auth::compute_csrf_token(&token)),
                        session_token: Some(token),
                    });
                }
            }
        }
    }

    // ─── 2. EXTRACT FLASH EARLY ────────────────────────────
    let flash_encoded = req.headers().get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| crate::auth::parse_cookie_header(h, crate::auth::FLASH_COOKIE));

    let mut flash_value: Option<serde_json::Value> = None;
    let mut flash_html = String::new();
    let mut expire_cookie = false;

    if let Some(encoded) = &flash_encoded {
        if let Some(flash) = crate::auth::decode_flash(encoded) {
            let variant = flash.get("variant").and_then(|v| v.as_str()).unwrap_or("success");
            let icon = flash.get("icon").and_then(|v| v.as_str()).unwrap_or("✅");
            let title = flash.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let description = flash.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let border_color = match variant { "success" => "#00ff88", "error" => "#ff4444", "warning" => "#ffaa00", _ => "#00f5ff" };
            flash_html = format!(
                r#"<div class="vlo-flash" style="position:fixed;top:20px;right:20px;z-index:99999;padding:16px 22px;border-radius:10px;background:#1a1a2e;border-left:4px solid {};color:#fff;box-shadow:0 8px 24px rgba(0,0,0,0.4);display:flex;align-items:center;gap:12px;max-width:380px;animation:vloFlashIn 0.35s ease"><span style="font-size:1.4rem">{}</span><div><strong style="display:block;font-size:0.9rem;margin-bottom:2px">{}</strong><span style="color:#aaa;font-size:0.78rem">{}</span></div></div><style>@keyframes vloFlashIn{{from{{opacity:0;transform:translateX(40px)}}to{{opacity:1;transform:translateX(0)}}}}</style>"#,
                border_color, icon, title, description
            );
            flash_value = Some(flash);
            expire_cookie = true;
        }
    }

    // ─── 3. CHECK AUTH GUARD (Using cached manifest) ───────
    let raw_query = uri.query().unwrap_or("");
    let next_decoded = raw_query.split('&').find_map(|pair| {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next()?;
        let value = parts.next().unwrap_or("");
        if key == "next" { Some(urlencoding::decode(value).unwrap_or_default().into_owned()) } else { None }
    });
    let next_ref = next_decoded.as_deref();

    if let Some(routes) = cache.manifest.as_object() {
        if let Some(route_config) = routes.get(path) {
            let guard = crate::auth::PageGuard {
                auth: route_config.get("auth").and_then(|v| v.as_bool()).unwrap_or(false),
                guest: route_config.get("guest").and_then(|v| v.as_bool()).unwrap_or(false),
                roles: route_config.get("roles").and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default(),
            };
            let user = auth.as_ref().and_then(|a| a.user.clone());
            if let Some(mut response) = crate::auth::check_page_guard(&guard, &user, path, next_ref) {
                if expire_cookie {
                    if let Ok(val) = axum::http::HeaderValue::from_str(&crate::auth::expire_flash_cookie()) {
                        response.headers_mut().append(axum::http::header::SET_COOKIE, val);
                    }
                }
                return response;
            }
        }
    }

    // ─── 4. FETCH PAGE FROM MEMORY CACHE ───────────────────
    let lookup_path = if path == "/" { "/".to_string() } else { path.to_string() };
    
    if let Some(html_arc) = cache.pages.get(&lookup_path) {
        let html = html_arc.as_ref(); // &str (Zero-allocation lookup!)
        
        let query: std::collections::HashMap<String, String> = uri.query().unwrap_or("").split('&')
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next()?;
                let value = parts.next().unwrap_or("");
                Some((key.to_string(), urlencoding::decode(value).unwrap_or_else(|_| value.into()).to_string()))
            }).collect();

        let mut context: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
        for (key, value) in &query {
            if let Ok(n) = value.parse::<i64>() { context.insert(key.clone(), serde_json::Value::Number(n.into())); }
            else { context.insert(key.clone(), serde_json::Value::String(value.clone())); }
        }

        let cfg = crate::auth::auth_config();
        context.insert("auth_identifier_field".to_string(), serde_json::Value::String(cfg.identifier_field.clone()));
        context.insert("auth_password_field".to_string(), serde_json::Value::String(cfg.password_field.clone()));

        if let Some(auth_user) = &auth {
            if let Some(user) = &auth_user.user {
                context.insert("logged_in".to_string(), serde_json::Value::Bool(true));
                context.insert("user_name".to_string(), serde_json::Value::String(user.name.clone()));
                context.insert("user_role".to_string(), serde_json::Value::String(user.role.clone()));
                context.insert("user_email".to_string(), serde_json::Value::String(user.email.clone()));
            }
        }

        if let Some(flash) = &flash_value {
            context.insert("flash_messages".to_string(), serde_json::json!([flash]));
            if let Some(obj) = flash.as_object() {
                for (key, value) in obj {
                    context.insert(format!("flash_{}", key), value.clone());
                }
                if let Some(v) = obj.get("variant") { context.insert("variant".to_string(), v.clone()); }
                if let Some(v) = obj.get("icon") { context.insert("icon".to_string(), v.clone()); }
                if let Some(v) = obj.get("title") { context.insert("title".to_string(), v.clone()); }
                if let Some(v) = obj.get("description") { context.insert("description".to_string(), v.clone()); }
                let desc_len = obj.get("description").and_then(|v| v.as_str()).map(|s| s.chars().count()).unwrap_or(0);
                let duration = if desc_len < 30 { "short" } else if desc_len < 80 { "medium" } else { "long" };
                context.insert("duration".to_string(), serde_json::Value::String(duration.to_string()));
            }
        }

        if !context.contains_key("limit") { context.insert("limit".to_string(), serde_json::Value::Number(20.into())); }
        if !context.contains_key("page") { context.insert("page".to_string(), serde_json::Value::Number(1.into())); }
        if !context.contains_key("order") { context.insert("order".to_string(), serde_json::Value::String("asc".to_string())); }
        if let (Some(p), Some(l)) = (context.get("page").and_then(|v| v.as_i64()), context.get("limit").and_then(|v| v.as_i64())) {
            let offset = (p.max(1) - 1) * l.max(1);
            context.insert("offset".to_string(), serde_json::Value::Number(offset.into()));
            context.insert("prev_page".to_string(), serde_json::Value::Number((p.max(1) - 1).max(1).into()));
            context.insert("next_page".to_string(), serde_json::Value::Number((p + 1).into()));
        }

        // ─── 5. RESOLVE DATA SOURCES ───────────────────
        let (html_with_data, computed_vars) = crate::router::resolve_data_sources(html, &context);
        let mut context = context;
        for (key, value) in computed_vars { context.insert(key, value); }

        // ─── 6. DYNAMIC COMPONENT RENDERING ────────────
        let mut render_page = crate::state::RenderedPage {
            html: html_with_data,
            styles: Vec::new(),
            template_context: context.clone(),
            used_modules: std::collections::HashSet::new(),
        };
        for _ in 0..20 {
            let previous = render_page.html.clone();
            let current_html = render_page.html.clone();
            render_page.html = crate::component::render_components(&current_html, &mut render_page);
            if render_page.html == previous { break; }
        }

        // ─── 7. FINAL CONTROL FLOW ─────────────────────
        let mut final_html = crate::template::render_control_flow(&render_page.html, &render_page.template_context);

        // ─── 8. RUNTIME PLACEHOLDER + SCRIPT INJECTION ──
        let csrf_token = auth.as_ref().and_then(|a| a.csrf_token.clone()).unwrap_or_default();
        final_html = final_html.replace("__VLO_CSRF_PLACEHOLDER__", &csrf_token);
        final_html = final_html.replace(r#"<div id="__VLO_FLASH_PLACEHOLDER__"></div>"#, &flash_html);

        if final_html.contains("class=\"vlo-sync\"") || final_html.contains("data-channel=") {
            if !final_html.contains("__VLO_SSE__") {
                let sse_script = r#"<script>
(function(){
  if(window.__VLO_SSE__) return;
  window.__VLO_SSE__ = new EventSource("/__vlo_sse");
  window.__VLO_SSE__.onmessage = function(e) {
    try {
      var data = JSON.parse(e.data);
      var els = document.querySelectorAll('[data-channel="' + data.channel + '"]');
      for (var i = 0; i < els.length; i++) {
        if (typeof data.value === 'object' && data.value !== null) {
          window.dispatchEvent(new CustomEvent('vlo:mutation'));
        } else {
          els[i].innerHTML = data.value;
          els[i].classList.add('vlo-sync-updated');
          (function(el){ setTimeout(function(){ el.classList.remove('vlo-sync-updated'); }, 600); })(els[i]);
        }
      }
    } catch(err) { console.error('VLO SSE Error:', err); }
  };
  window.__VLO_SSE__.onerror = function(e) { console.error('VLO SSE connection error:', e); };
})();
</script>"#;
                if let Some(i) = final_html.rfind("</body>") {
                    final_html.insert_str(i, &format!("{}\n", sse_script));
                }
            }
        }

        let mut response = (axum::http::StatusCode::OK, axum::response::Html(final_html)).into_response();
        if expire_cookie {
            if let Ok(val) = axum::http::HeaderValue::from_str(&crate::auth::expire_flash_cookie()) {
                response.headers_mut().append(axum::http::header::SET_COOKIE, val);
            }
        }
        response
    } else {
        // 404 handling (fallback to disk read only if 404.html is missing from cache)
        let not_found = build_dir.join("404.html");
        match std::fs::read_to_string(not_found) {
            Ok(html) => (axum::http::StatusCode::NOT_FOUND, axum::response::Html(html)).into_response(),
            Err(_) => (axum::http::StatusCode::NOT_FOUND, axum::response::Html("404 - Page Not Found".to_string())).into_response(),
        }
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");
    println!("\n⚡ Shutting down VLO dev server...");
}

// Add this handler function:
async fn broadcast_handler(
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let channel = payload.get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    
    let value = payload.get("value")
        .cloned()
        .unwrap_or(serde_json::json!(null));
    
    crate::router::broadcast_live(channel, &value);
    
    (StatusCode::OK, axum::Json(serde_json::json!({"success": true})))
}

async fn cache_middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let mut response = next.run(req).await;

    // 1. Static assets and uploads are immutable (cached for 1 year)
    if path.starts_with("/static/") || path.starts_with("/uploads/") {
        response.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    // 2. Fallback for direct asset extensions
    else if path.ends_with(".css") || path.ends_with(".js") || path.ends_with(".png")
         || path.ends_with(".jpg") || path.ends_with(".svg") || path.ends_with(".woff2") {
        response.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    // 3. API endpoints: private, no-cache (prevents shared-cache leakage)
    else if path.starts_with("/api/") {
        response.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
    }
    // 4. HTML pages: private, no-cache (allows bfcache, still revalidates)
    else {
        response.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
    }

    response
}

pub fn build(release: bool) -> Result<(), String> {
    crate::state::set_building(true); // ← ADD THIS LINE
    if release {
        println!("⚡ VLO release build...");
    } else {
        println!("⚡ VLO production build...");
    }

    let root = get_project_root();
    let pages = root.join("pages");
    let public = root.join("public");
    let build_dir = root.join(".vlo").join("build");

    if build_dir.exists() {
        fs::remove_dir_all(&build_dir)
            .expect("Failed to clean previous build");
    }

    fs::create_dir_all(build_dir.join("static"))
        .expect("Failed to create .vlo/build directory");

    // Collect all route guards into a single manifest
    let mut routes_manifest = serde_json::Map::new();

    // Recursive page builder
    fn build_pages(
        dir: &Path,
        pages_root: &Path,
        build_dir: &Path,
        manifest: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), String> {
        let entries = fs::read_dir(dir)
            .map_err(|e| format!("Failed to read pages directory: {}", e))?;

        for entry in entries.flatten() {
            let file_path = entry.path();

            if file_path.is_dir() {
                build_pages(&file_path, pages_root, build_dir, manifest)?;
                continue;
            }

            if file_path.extension().and_then(|e| e.to_str()) != Some("vlo") {
                continue;
            }

            let relative_page_path = file_path
                .strip_prefix(pages_root)
                .map_err(|e| format!("Failed to resolve page path: {}", e))?
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");

            let content = fs::read_to_string(&file_path)
                .unwrap_or_default();

            // Extract guard BEFORE moving content
            let page_guard = crate::auth::extract_page_guard(&content);

            let rendered = crate::router::render_vlo_for_build_at(
                &relative_page_path,
                content,
            );

            let html = crate::router::wrap_html(
                &relative_page_path,
                &rendered,
                true,
            );

            let output = if relative_page_path == "home"
                || relative_page_path == "index"
            {
                build_dir.join("index.html")
            } else {
                build_dir.join(format!("{}.html", relative_page_path))
            };

            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create build directory: {}", e))?;
            }

            fs::write(&output, html)
                .map_err(|e| format!("Failed to write generated HTML: {}", e))?;

            // ─── ADD TO ROUTES MANIFEST ────────────────────
            if let Some(guard) = page_guard {
                let route_path = if relative_page_path == "home" || relative_page_path == "index" {
                    "/".to_string()
                } else {
                    format!("/{}", relative_page_path)
                };
                manifest.insert(route_path, serde_json::json!({
                    "auth": guard.auth,
                    "guest": guard.guest,
                    "roles": guard.roles
                }));
            }
            // ──────────────────────────────────────────────

            println!("  ├─ Generated: {}", output.display());
        }

        Ok(())
    }

    build_pages(&pages, &pages, &build_dir, &mut routes_manifest)?;

    // Write single routes.json manifest
    if !routes_manifest.is_empty() {
        let guard_count = routes_manifest.len(); // ← Get count BEFORE moving
        let manifest_path = build_dir.join("routes.json");
        let manifest_json = serde_json::to_string_pretty(&serde_json::Value::Object(routes_manifest))
            .map_err(|e| format!("Failed to serialize routes manifest: {}", e))?;
        fs::write(&manifest_path, manifest_json)
            .map_err(|e| format!("Failed to write routes manifest: {}", e))?;
        println!("  ├─ Routes manifest: {} guards", guard_count);
    }

    if public.exists() {
        copy_dir_all(&public, &build_dir.join("static"))
            .expect("Failed to copy static assets");
        println!("  └─ Copied static assets");
    }

    println!("⚡ Build completed successfully!");
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
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
        "netlify" => vec![
            "netlify-cli",
            "deploy",
            "--dir=.vlo/build",
            "--prod",
        ],
        "vercel" => vec![
            "vercel",
            "deploy",
            ".vlo/build",
            "--prod",
        ],
        "cloudflare" | "pages" => vec![
            "wrangler",
            "pages",
            "deploy",
            ".vlo/build",
        ],
        "railway" => vec!["@railway/cli", "up"],
        _ => return Err(format!("Unsupported provider '{}'.", provider)),
    };

    let working_dir = if provider == "railway" {
        &build_dir
    } else {
        &root
    };

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
    .map_err(|e| format!("Failed to execute deployment command: {}", e))?;

    if !status.success() {
        return Err(format!(
            "Deployment exited with status: {}",
            status
        ));
    }

    println!("⚡ Deployment completed successfully!");

    Ok(())
}

pub fn js_string_literal(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

// ─── STATIC REGEXES (Compiled once at startup) ───────────────────────
static RE_DEL: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)v-delete\s*=\s*["']([^"']+)["']([^>]*?)>"#).unwrap());
static RE_PUT: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)v-put\s*=\s*["']([^"']+)["']([^>]*?)>"#).unwrap());
static RE_POST: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)v-post\s*=\s*["']([^"']+)["']([^>]*?)>"#).unwrap());
static RE_STRIP: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"(?is)\s+v-(?:put|delete|post|prompt|param|confirm)\s*=\s*["'][^"']*["']"#).unwrap());

pub fn strip_vlo_directive_attrs(attrs: &str) -> String {
    RE_STRIP.replace_all(attrs, "").into_owned()
}

pub fn resolve_directives(source: &str) -> String {
    let mut result = source.to_string();

    // Zero-allocation attribute value parser
    fn attr_val<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
        let rest = attrs[attrs.find(name)? + name.len()..].trim_start().strip_prefix('=')?.trim_start();
        let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &rest[1..];
        rest.find(q).map(|e| &rest[..e])
    }

    // Shared JS generator for POST/PUT forms (already handles inline errors nicely)
    // Shared JS generator for POST/PUT forms
    fn form_submit_js(method: &str, url_js: &str) -> String {
        format!(
            "return(function(e){{
                e.preventDefault();
                var f=e.currentTarget;
                if(!f.checkValidity()){{f.reportValidity();return false}}
                var mp=f.enctype==='multipart/form-data';
                var h={{'X-CSRF-Token':document.querySelector('meta[name=\"csrf-token\"]')?.content||''}};
                if(!mp)h['Content-Type']='application/x-www-form-urlencoded';
                var bd=mp?new FormData(f):new URLSearchParams(new FormData(f));
                var un=new URLSearchParams(window.location.search).get('next');
                if(un&&!bd.has('next')){{bd.append('next',un)}}
                fetch({url},{{credentials:'same-origin',method:'{method}',headers:h,body:bd}})
                .then(async function(r){{
                    var d;try{{d=await r.json()}}catch(x){{d={{}}}}
                    if(!r.ok){{
                        // 🔥 CHECK FOR ACTIVATION REQUIRED FIRST
                        if(d.activation_required === true) {{
                            var old=f.querySelector('.vlo-inline-error');
                            if(old)old.remove();
                            var err=document.createElement('div');
                            err.className='auth-alert vlo-inline-error'; 
                            err.style.cssText='display:flex;flex-direction:column;gap:10px;margin-bottom:15px;padding:16px;background:rgba(0,245,255,0.05);border:1px solid rgba(0,245,255,0.25);border-radius:8px;font-size:0.9rem;';
                            err.innerHTML='<div style=\"display:flex;align-items:center;gap:10px\"><span style=\"background:rgba(0,245,255,0.1);color:#00f5ff;font-weight:bold;font-size:1.2rem;width:24px;height:24px;display:grid;place-items:center;border-radius:7px\">i</span><p style=\"margin:0;color:#80f0ff\">'+(d.error||'Account is not active')+' Please check your email.</p></div><button type=\"button\" onclick=\"openResendActivationModal()\" style=\"margin-top:8px;padding:8px 12px;background:#00f5ff;color:#000;border:none;border-radius:6px;font-weight:700;cursor:pointer;font-size:0.8rem;\">Resend Activation Email →</button>';
                            var btn=f.querySelector('button[type=\"submit\"]');
                            if(btn) btn.parentNode.insertBefore(err, btn);
                            else f.appendChild(err);
                            return; // Stop further processing, do NOT throw error
                        }}
                        throw new Error(d.error||d.details||d.message||'Request failed')
                    }}
                    if(d.redirect){{window.location.href=d.redirect;return;}}
                    f.reset();
                    window.dispatchEvent(new CustomEvent('vlo:mutation'));
                    var m=f.closest('.vlo-modal-overlay');if(m)m.classList.remove('active')
                }})
                .catch(function(e){{
                    console.error('[VLO {method}]', e);
                    var old=f.querySelector('.vlo-inline-error');
                    if(old)old.remove();
                    var err=document.createElement('div');
                    err.className='auth-alert vlo-inline-error'; 
                    err.style.cssText='display:flex;align-items:center;gap:10px;color:#ff4444;margin-bottom:15px;padding:12px;background:rgba(255,68,68,0.1);border:1px solid rgba(255,68,68,0.3);border-radius:8px;font-size:0.9rem;';
                    err.innerHTML='<span style=\"font-weight:bold;font-size:1.2rem\">!</span><p style=\"margin:0;color:#ff4444\">'+(e.message||'Request failed')+'</p>';
                    var btn=f.querySelector('button[type=\"submit\"]');
                    if(btn) btn.parentNode.insertBefore(err, btn);
                    else f.appendChild(err);
                }});
                return false
            }})(event)",
            url = url_js,
            method = method
        )
    }

    // 🔥 NEW: Shared floating toast for non-form button errors (replaces native alert)
    // Note: Braces are doubled ({{ }}) so Rust's format! macro doesn't treat them as placeholders.
    let error_toast = "var t=document.createElement('div');t.className='vlo-flash';t.style.cssText='position:fixed;top:20px;right:20px;z-index:99999;padding:16px 22px;border-radius:10px;background:#1a1a2e;border-left:4px solid #ff4444;color:#fff;box-shadow:0 8px 24px rgba(0,0,0,0.4);display:flex;align-items:center;gap:12px;max-width:380px;animation:vloFlashIn 0.35s ease';t.innerHTML='<span style=\\\"font-size:1.4rem\\\">⚠️</span><div><strong style=\\\"display:block;font-size:0.9rem;margin-bottom:2px\\\">Error</strong><span style=\\\"color:#aaa;font-size:0.78rem\\\">'+(e.message||'Request failed')+'</span></div>';document.body.appendChild(t);setTimeout(function(){{t.style.transition='opacity 0.4s';t.style.opacity='0';setTimeout(function(){{t.remove();}},400);}},4000);";

    // ============================================================
    // v-delete
    // ============================================================
    result = RE_DEL.replace_all(&result, |caps: &regex::Captures| {
        let tag = caps.get(1).unwrap().as_str();
        let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let url_raw = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");
        let url = url_raw.replace("|ajax", "");
        let all_attrs = format!("{} {}", attrs_before, attrs_after);
        let confirm_js = if let Some(msg) = attr_val(&all_attrs, "v-confirm") {
            format!("confirm({})", js_string_literal(msg))
        } else { "true".to_string() };
        let clean_before = strip_vlo_directive_attrs(attrs_before);
        let clean_after = strip_vlo_directive_attrs(attrs_after);
        let url_js = js_string_literal(&url);
        let all_clean = format!("{} {}", clean_before.trim(), clean_after.trim()).trim().to_string();
        
        let onclick = format!(
            "return(function(){{if({confirm}){{fetch({url},{{credentials:'same-origin',method:'DELETE',headers:{{'X-CSRF-Token':document.querySelector('meta[name=\"csrf-token\"]')?.content||''}}}}).then(async function(r){{var d;try{{d=await r.json()}}catch(x){{d={{}}}}if(!r.ok){{throw new Error(d.error||d.details||d.message||'Request failed')}}window.dispatchEvent(new CustomEvent('vlo:mutation'))}}).catch(function(e){{console.error('[VLO DELETE]',e);{error_toast}}})}}return false}})()",
            confirm = confirm_js,
            url = url_js
        );
        let onclick_attr = escape_html_attribute(&onclick);
        format!("<{} {} onclick=\"{}\">", tag, all_clean, onclick_attr)
    }).into_owned();

    // ============================================================
    // v-put
    // ============================================================
    result = RE_PUT.replace_all(&result, |caps: &regex::Captures| {
        let tag = caps.get(1).unwrap().as_str();
        let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let url_raw = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");
        let url = url_raw.replace("|ajax", "");
        let clean_before = strip_vlo_directive_attrs(attrs_before);
        let clean_after = strip_vlo_directive_attrs(attrs_after);
        let url_js = js_string_literal(&url);
        let all_clean = format!("{} {}", clean_before.trim(), clean_after.trim()).trim().to_string();
        
        if tag.eq_ignore_ascii_case("form") {
            let onsubmit = form_submit_js("PUT", &url_js);
            let onsubmit_attr = escape_html_attribute(&onsubmit);
            format!("<{} {} onsubmit=\"{}\">", tag, all_clean, onsubmit_attr)
        } else {
            let all_attrs = format!("{} {}", attrs_before, attrs_after);
            let param = attr_val(&all_attrs, "v-param").unwrap_or("value");
            let prompt = attr_val(&all_attrs, "v-prompt").unwrap_or("Enter new value:");
            let prompt_js = js_string_literal(prompt);
            let param_js = js_string_literal(param);
            
            let onclick = format!(
                "return(function(){{var v=prompt({prompt});if(v!==null){{fetch({url},{{credentials:'same-origin',method:'PUT',headers:{{'Content-Type':'application/json','X-CSRF-Token':document.querySelector('meta[name=\"csrf-token\"]')?.content||''}},body:JSON.stringify({{{param}:v}})}}).then(async function(r){{var d;try{{d=await r.json()}}catch(x){{d={{}}}}if(!r.ok){{throw new Error(d.error||d.details||d.message||'Request failed')}}window.dispatchEvent(new CustomEvent('vlo:mutation'))}}).catch(function(e){{console.error('[VLO PUT]',e);{error_toast}}})}}return false}})()",
                prompt = prompt_js,
                url = url_js,
                param = param_js
            );
            let onclick_attr = escape_html_attribute(&onclick);
            format!("<{} {} onclick=\"{}\">", tag, all_clean, onclick_attr)
        }
    }).into_owned();

    // ============================================================
    // v-post
    // ============================================================
    result = RE_POST.replace_all(&result, |caps: &regex::Captures| {
        let tag = caps.get(1).unwrap().as_str();
        let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let url_raw = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");
        let url = url_raw.replace("|ajax", "");
        let clean_before = strip_vlo_directive_attrs(attrs_before);
        let clean_after = strip_vlo_directive_attrs(attrs_after);
        let url_js = js_string_literal(&url);
        let all_clean = format!("{} {}", clean_before.trim(), clean_after.trim()).trim().to_string();
        
        if tag.eq_ignore_ascii_case("form") {
            let onsubmit = form_submit_js("POST", &url_js);
            let onsubmit_attr = escape_html_attribute(&onsubmit);
            format!("<{} {} onsubmit=\"{}\">", tag, all_clean, onsubmit_attr)
        } else {
            format!("<{} {}>", tag, all_clean)
        }
    }).into_owned();

    vlo_debug!("🧩 [VLO DIRECTIVES] Resolved HTML: {} chars, {} rows", result.len(), result.lines().count());
    result
}