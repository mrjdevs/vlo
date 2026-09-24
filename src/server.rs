use crate::{
    api::{api_handler_id, api_handler_path, api_handler_root},
    files_api::{delete_file, download_file, get_file, serve_file, upload_file},
    router::{hmr_handler, home_handler, not_found_handler, page_handler, watch_files, resolve_data_sources},
    state::{self, get_project_root},
    template::{escape_html_attribute, render_control_flow}};
use axum::{
    response::{Html, IntoResponse},
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
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::broadcast;
use tower_http::{compression::CompressionLayer, services::ServeDir};
use axum::middleware::{self, Next};
use axum::extract::Request;
use axum::response::Response;
use axum::http::header::{CACHE_CONTROL, HeaderValue};


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
pub async fn dev(host: Option<&str>, port: Option<u16>) -> Result<(), String> {
    state::set_app_mode(state::AppMode::Development);
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
        .route("/uploads/*path", get(serve_file))
        .route("/api/files/upload", axum::routing::post(upload_file))
        .route("/api/files/:id/download", get(download_file))
        .route(
            "/api/files/:id",
            get(get_file).delete(delete_file),
        )
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
        // ─── Auth routes ─────────────────────────────────────
        .route("/api/auth/login", axum::routing::post(auth::login_handler))
        .route("/api/auth/logout", axum::routing::post(auth::logout_handler).get(auth::logout_handler))
        .route("/api/auth/me", axum::routing::get(auth::me_handler))
        // ─────────────────────────────────────────────────────
        .route("/__vlo_hmr", get(move || hmr_handler(tx)))
        .nest_service("/static", ServeDir::new(public_path_service))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn(cache_middleware))
        // Middleware execution order: session -> api_auth -> csrf
        // Layers execute in REVERSE order (last added runs first)
        .layer(axum::middleware::from_fn(auth::csrf_middleware))        // Runs 3rd
        .layer(axum::middleware::from_fn(auth::api_auth_middleware))    // Runs 2nd
        .layer(axum::middleware::from_fn(auth::session_middleware))     // Runs 1st (MUST BE LAST)
        .fallback(not_found_handler);

    // CLI arguments override .env values.
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
                "Failed to start VLO dev server.\n   Address: {}\n   Error: {}\n   Try another port with: vlo dev --port 3001",
                addr,
                error
            ));
        }
    };

    println!("⚡ VLO dev server: http://{}", addr);

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        return Err(format!(
            "VLO dev server stopped unexpectedly.\n   Error: {}",
            error
        ));
    }

    Ok(())
}

pub async fn serve(host: Option<&str>, port: Option<u16>) -> Result<(), String> {
    state::set_app_mode(state::AppMode::Production);

    let root = get_project_root();
    let build_dir = root.join(".vlo").join("build");
    let static_dir = build_dir.join("static");

    if !build_dir.exists() {
        return Err(
            "Production build not found.\n   Run `vlo build` first.".to_string()
        );
    }

    let app = Router::new()
        .route("/api/files/upload", axum::routing::post(upload_file))
        .route("/api/files/:id/download", get(download_file))
        .route("/api/files/:id", get(get_file).delete(delete_file))
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
        // ─── Auth routes ─────────────────────────────────────
        .route("/api/auth/login", axum::routing::post(auth::login_handler))
        .route("/api/auth/logout", axum::routing::post(auth::logout_handler).get(auth::logout_handler))
        .route("/api/auth/me", axum::routing::get(auth::me_handler))
        // ─────────────────────────────────────────────────────
        .route("/uploads/*path", get(serve_file))
        .nest_service("/static", ServeDir::new(static_dir))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn(cache_middleware))
        // Middleware execution order: session -> api_auth -> csrf
        // Layers execute in REVERSE order (last added runs first)
        .layer(axum::middleware::from_fn(auth::csrf_middleware))        // Runs 3rd
        .layer(axum::middleware::from_fn(auth::api_auth_middleware))    // Runs 2nd
        .layer(axum::middleware::from_fn(auth::session_middleware))     // Runs 1st (MUST BE LAST)
        .fallback(move |uri: axum::http::Uri| {
            serve_build_page(build_dir.clone(), uri)
        });

    // CLI arguments override .env values.
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
                "Failed to start VLO production server.\n\
                 Address: {}\n\
                 Error: {}\n\
                 Try another port with: vlo serve --port 3001",
                addr, error
            ));
        }
    };

    println!("⚡ VLO production server: http://{}", addr);

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        return Err(format!(
            "VLO production server stopped unexpectedly.\n   Error: {}",
            error
        ));
    }

    Ok(())
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
) -> impl IntoResponse {
    let path = uri.path();

    let filename = if path == "/" {
        "index.html".to_string()
    } else {
        let name = path.trim_start_matches('/');

        if name.contains('/')
            || name.contains('\\')
            || name.contains("..")
        {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Html("404 - Page Not Found".to_string()),
            )
                .into_response();
        }

        format!("{}.html", name)
    };

    let file = build_dir.join(&filename);

    match fs::read_to_string(&file) {
        Ok(html) => {
            let query = uri
                .query()
                .unwrap_or("")
                .split('&')
                .filter_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    let key = parts.next()?;
                    let value = parts.next().unwrap_or("");

                    Some((
                        key.to_string(),
                        urlencoding::decode(value)
                            .unwrap_or_else(|_| value.into())
                            .to_string(),
                    ))
                })
                .collect::<std::collections::HashMap<_, _>>();

            let mut context = std::collections::HashMap::new();

            for (key, value) in &query {
                // Parse numeric query parameters as integers
                if let Ok(n) = value.parse::<i64>() {
                    context.insert(key.clone(), serde_json::Value::Number(n.into()));
                } else {
                    context.insert(key.clone(), serde_json::Value::String(value.clone()));
                }
            }

            // Resolve data-source blocks from the live database.
                let (html, computed_vars) = resolve_data_sources(&html, &context);
                let mut context = context;
                for (key, value) in computed_vars {
                    context.insert(key, value);
                }

            let rendered = render_control_flow(
                &html,
                &context,
            );

            let rendered = rendered.replacen(
                "</body>",
                &format!(
                    r#"<script>
            history.replaceState(null, "", {});
            </script>
            </body>"#,
                    serde_json::to_string(path).unwrap()
                ),
                1,
            );

            (
                axum::http::StatusCode::OK,
                Html(rendered),
            )
                .into_response()
        }

        Err(_) => {
            let not_found = build_dir.join("404.html");

            match fs::read_to_string(not_found) {
                Ok(html) => (
                    axum::http::StatusCode::NOT_FOUND,
                    Html(html),
                )
                    .into_response(),

                Err(_) => (
                    axum::http::StatusCode::NOT_FOUND,
                    Html("404 - Page Not Found".to_string()),
                )
                    .into_response(),
            }
        }
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");
    println!("\n⚡ Shutting down VLO dev server...");
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

    // Recursive page builder
    fn build_pages(
        dir: &Path,
        pages_root: &Path,
        build_dir: &Path,
    ) -> Result<(), String> {
        let entries = fs::read_dir(dir)
            .map_err(|e| format!("Failed to read pages directory: {}", e))?;

        for entry in entries.flatten() {
            let file_path = entry.path();

            // Recurse into subdirectories (e.g., pages/admin/)
            if file_path.is_dir() {
                build_pages(&file_path, pages_root, build_dir)?;
                continue;
            }

            if file_path.extension().and_then(|e| e.to_str()) != Some("vlo") {
                continue;
            }

            // Calculate relative page path: "docs", "admin/dashboard", etc.
            let relative_page_path = file_path
                .strip_prefix(pages_root)
                .map_err(|e| format!("Failed to resolve page path: {}", e))?
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");

            let content = fs::read_to_string(&file_path)
                .unwrap_or_default();

            // Pass the page path so layout hierarchy is applied!
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

            // Create subdirectories in build output if needed
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| {
                        format!("Failed to create build directory: {}", e)
                    })?;
            }

            fs::write(&output, html)
                .map_err(|e| {
                    format!("Failed to write generated HTML: {}", e)
                })?;

            println!("  ├─ Generated: {}", output.display());
        }

        Ok(())
    }

    build_pages(&pages, &pages, &build_dir)?;

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

pub fn strip_vlo_directive_attrs(attrs: &str) -> String {
    let re = regex::Regex::new(
        r#"(?is)\s+v-(?:put|delete|prompt|param|confirm)\s*=\s*["'][^"']*["']"#,
    )
    .unwrap();
    re.replace_all(attrs, "").into_owned()
}

pub fn resolve_directives(source: &str) -> String {
    let mut result = source.to_string();

    // ============================================================
    // v-delete
    // ============================================================
    let re_del = regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)v-delete\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    ).unwrap();

    result = re_del.replace_all(&result, |caps: &regex::Captures| {
        let tag = caps.get(1).unwrap().as_str();
        let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let url = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");
        let all_attrs = format!("{} {}", attrs_before, attrs_after);

        let confirm_re = regex::Regex::new(r#"(?is)v-confirm\s*=\s*["']([^"']*)["']"#).unwrap();
        let confirm_js = if let Some(c) = confirm_re.captures(&all_attrs) {
            let msg = c.get(1).map(|m| m.as_str()).unwrap_or("");
            format!("confirm({})", js_string_literal(msg))
        } else { "true".to_string() };

        let clean_before = strip_vlo_directive_attrs(attrs_before);
        let clean_after = strip_vlo_directive_attrs(attrs_after);
        let url_js = js_string_literal(url);

        let onclick = format!(
            "if({}){{fetch({},{{method:'DELETE',headers:{{'X-CSRF-Token':document.querySelector('meta[name=\"csrf-token\"]')?.content||''}}}}).then(async r=>{{let d;try{{d=await r.json()}}catch(_){{d={{}}}}if(!r.ok){{throw new Error(d.message||d.details||d.error||'Request failed')}}window.location.reload()}}).catch(e=>{{console.error('[VLO DELETE]',e);window.location.reload()}})}}",
            confirm_js, url_js
        );

        let onclick_attr = escape_html_attribute(&onclick);
        format!("<{} {} onclick=\"{}\">", tag, format!("{} {}", clean_before.trim(), clean_after.trim()).trim(), onclick_attr)
    }).into_owned();

    // ============================================================
    // v-put
    // ============================================================
    let re_put = regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)v-put\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    ).unwrap();

    result = re_put.replace_all(&result, |caps: &regex::Captures| {
        let tag = caps.get(1).unwrap().as_str();
        let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let url = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");
        let all_attrs = format!("{} {}", attrs_before, attrs_after);
        let clean_before = strip_vlo_directive_attrs(attrs_before);
        let clean_after = strip_vlo_directive_attrs(attrs_after);
        let url_js = js_string_literal(url);

        if tag.eq_ignore_ascii_case("form") {
            let onsubmit = format!(
                "event.preventDefault();fetch({},{{method:'PUT',headers:{{'Content-Type':'application/x-www-form-urlencoded','X-CSRF-Token':document.querySelector('meta[name=\"csrf-token\"]')?.content||''}},body:new URLSearchParams(new FormData(event.currentTarget))}}).then(async r=>{{let d;try{{d=await r.json()}}catch(_){{d={{}}}}if(!r.ok){{throw new Error(d.message||d.details||d.error||'Request failed')}}window.location.reload()}}).catch(e=>{{console.error('[VLO PUT]',e);window.location.reload()}});return false",
                url_js
            );
            let onsubmit_attr = escape_html_attribute(&onsubmit);
            format!("<{} {} onsubmit=\"{}\">", tag, format!("{} {}", clean_before.trim(), clean_after.trim()).trim(), onsubmit_attr)
        } else {
            let param_re = regex::Regex::new(r#"(?is)v-param\s*=\s*["']([^"']+)["']"#).unwrap();
            let param = param_re.captures(&all_attrs).and_then(|c| c.get(1)).map(|m| m.as_str()).unwrap_or("value");
            let prompt_re = regex::Regex::new(r#"(?is)v-prompt\s*=\s*["']([^"']*)["']"#).unwrap();
            let prompt = prompt_re.captures(&all_attrs).and_then(|c| c.get(1)).map(|m| m.as_str()).unwrap_or("Enter new value:");
            let prompt_js = js_string_literal(prompt);
            let param_js = js_string_literal(param);

            let onclick = format!(
                "let v=prompt({});if(v!==null){{fetch({},{{method:'PUT',headers:{{'Content-Type':'application/json','X-CSRF-Token':document.querySelector('meta[name=\"csrf-token\"]')?.content||''}},body:JSON.stringify({{{}:v}})}}).then(async r=>{{if(!r.ok){{let d;try{{d=await r.json()}}catch(_){{d={{}}}};throw new Error(d.details||d.error||'Request failed')}}window.location.reload()}}).catch(e=>{{console.error('[VLO PUT]',e);window.location.reload()}})}}",
                prompt_js, url_js, param_js
            );
            let onclick_attr = escape_html_attribute(&onclick);
            format!("<{} {} onclick=\"{}\">", tag, format!("{} {}", clean_before.trim(), clean_after.trim()).trim(), onclick_attr)
        }
    }).into_owned();

    // ============================================================
    // v-post
    // ============================================================
    let re_post = regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)v-post\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    ).unwrap();

    result = re_post.replace_all(&result, |caps: &regex::Captures| {
        let tag = caps.get(1).unwrap().as_str();
        let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let url = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");
        let clean_before = strip_vlo_directive_attrs(attrs_before);
        let clean_after = strip_vlo_directive_attrs(attrs_after);
        let url_js = js_string_literal(url);

        if tag.eq_ignore_ascii_case("form") {
            let onsubmit = format!(
                "event.preventDefault();fetch({},{{method:'POST',headers:{{'X-CSRF-Token':document.querySelector('meta[name=\"csrf-token\"]')?.content||''}},body:new FormData(event.currentTarget)}}).then(async r=>{{let d;try{{d=await r.json()}}catch(_){{d={{}}}}if(!r.ok){{throw new Error(d.message||d.details||d.error||'Request failed')}}window.location.reload()}}).catch(e=>{{console.error('[VLO POST]',e);window.location.reload()}});return false",
                url_js
            );
            let onsubmit_attr = escape_html_attribute(&onsubmit);
            format!("<{} {} onsubmit=\"{}\">", tag, format!("{} {}", clean_before.trim(), clean_after.trim()).trim(), onsubmit_attr)
        } else {
            format!("<{} {}>", tag, format!("{} {}", clean_before.trim(), clean_after.trim()).trim())
        }
    }).into_owned();

    vlo_debug!("🧩 [VLO DIRECTIVES] Resolved HTML: {} chars, {} rows", result.len(), result.lines().count());
    result
}