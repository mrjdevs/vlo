use crate::{
    api::{api_handler_id, api_handler_path, api_handler_root},
    files_api::{delete_file, download_file, get_file, serve_file, upload_file},
    router::{hmr_handler, home_handler, not_found_handler, page_handler, watch_files},
    state::{self, get_project_root},
    template::escape_html_attribute,
};
use axum::{
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use axum::extract::DefaultBodyLimit;
use clap::Subcommand;
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::broadcast;
use tower_http::{compression::CompressionLayer, services::ServeDir};

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
        #[arg(short, long, default_value = "3000")]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },

    Build {
        #[arg(long)]
        release: bool,
    },

    Serve {
        #[arg(short, long, default_value = "3000")]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },

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

pub async fn dev(host: &str, port: u16) -> Result<(), String> {
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
        .route(
            "/api/files/:id/download",
            get(download_file),
        )
        .route(
            "/api/files/:id",
            get(get_file)
                .delete(delete_file),
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
        .route("/__vlo_hmr", get(move || hmr_handler(tx)))
        .nest_service("/static", ServeDir::new(public_path_service))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .fallback(not_found_handler);

    let host_str = std::env::var("VLO_HOST").unwrap_or_else(|_| host.to_string());
    let port_str = std::env::var("VLO_PORT").unwrap_or_else(|_| port.to_string());
    let addr = format!("{}:{}", host_str, port_str);

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

pub async fn serve(host: &str, port: u16) -> Result<(), String> {
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
        .route("/uploads/*path", get(serve_file))
        .nest_service("/static", ServeDir::new(static_dir))
        .fallback(move |uri: axum::http::Uri| {
            serve_build_page(build_dir.clone(), uri)
        })
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(CompressionLayer::new());

    let host_str = std::env::var("VLO_HOST")
        .unwrap_or_else(|_| host.to_string());

    let port_str = std::env::var("VLO_PORT")
        .unwrap_or_else(|_| port.to_string());

    let addr = format!("{}:{}", host_str, port_str);

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
        Ok(html) => (
            axum::http::StatusCode::OK,
            Html(html),
        )
            .into_response(),

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

    if let Ok(entries) = fs::read_dir(&pages) {
        for entry in entries.flatten() {
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("vlo") {
                continue;
            }

            let stem = path
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .to_string();

            let content = fs::read_to_string(&path)
                .unwrap_or_default();

            let rendered = crate::router::render_vlo(content);
            let html = crate::router::wrap_html(&stem, &rendered);

            let output = if stem == "home" || stem == "index" {
                build_dir.join("index.html")
            } else {
                build_dir.join(format!("{}.html", stem))
            };

            fs::write(&output, html)
                .expect("Failed to write generated HTML");

            println!("  ├─ Generated: {}", output.display());
        }
    }

    if public.exists() {
        copy_dir_all(
            &public,
            &build_dir.join("static"),
        )
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
    )
    .unwrap();

    result = re_del
        .replace_all(&result, |caps: &regex::Captures| {
            let tag = caps.get(1).unwrap().as_str();
            let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let url = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");

            let all_attrs = format!("{} {}", attrs_before, attrs_after);

            let confirm_re =
                regex::Regex::new(r#"(?is)v-confirm\s*=\s*["']([^"']*)["']"#)
                    .unwrap();

            let confirm_js = if let Some(c) = confirm_re.captures(&all_attrs) {
                let msg = c.get(1).map(|m| m.as_str()).unwrap_or("");
                format!("confirm({})", js_string_literal(msg))
            } else {
                "true".to_string()
            };

            let clean_before = strip_vlo_directive_attrs(attrs_before);
            let clean_after = strip_vlo_directive_attrs(attrs_after);
            let url_js = js_string_literal(url);

            // Updated fetch handler with status & action query flags for toast notifications on deletion
            let onclick = format!(
                "if({}){{fetch({},{{method:'DELETE'}}).then(async r=>{{let d;try{{d=await r.json()}}catch(_){{d={{}}}}if(!r.ok){{throw new Error(d.message||d.details||d.error||'Request failed')}}window.location.href=window.location.pathname+'?status=success&action=deleted&message='+encodeURIComponent(d.message||'Delete operation successful.')}}).catch(e=>{{console.error('[VLO DELETE]',e);window.location.href=window.location.pathname+'?status=error&action=error&message='+encodeURIComponent(e.message)}})}}",
                confirm_js,
                url_js
            );
            let onclick_attr = escape_html_attribute(&onclick);

            format!(
                "<{} {} onclick=\"{}\">",
                tag,
                format!("{} {}", clean_before.trim(), clean_after.trim()).trim(),
                onclick_attr
            )
        })
        .into_owned();

    // ============================================================
    // v-put
    // ============================================================

    let re_put = regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)v-put\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    )
    .unwrap();

    result = re_put
        .replace_all(&result, |caps: &regex::Captures| {
            let tag = caps.get(1).unwrap().as_str();
            let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let url = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");

            let all_attrs = format!("{} {}", attrs_before, attrs_after);

            let clean_before = strip_vlo_directive_attrs(attrs_before);
            let clean_after = strip_vlo_directive_attrs(attrs_after);

            let url_js = js_string_literal(url);

            if tag.eq_ignore_ascii_case("form") {
                // Form submit handler with status & action query flags for toast notifications on update
                let onsubmit = format!(
                    "event.preventDefault();fetch({},{{method:'PUT',headers:{{'Content-Type':'application/x-www-form-urlencoded'}},body:new URLSearchParams(new FormData(event.currentTarget))}}).then(async r=>{{let d;try{{d=await r.json()}}catch(_){{d={{}}}}if(!r.ok){{throw new Error(d.message||d.details||d.error||'Request failed')}}window.location.href=window.location.pathname+'?status=success&action=updated&message='+encodeURIComponent(d.message||'Update operation successful.')}}).catch(e=>{{console.error('[VLO PUT]',e);window.location.href=window.location.pathname+'?status=error&action=error&message='+encodeURIComponent(e.message)}});return false",
                    url_js
                );

                let onsubmit_attr = escape_html_attribute(&onsubmit);

                format!(
                    "<{} {} onsubmit=\"{}\">",
                    tag,
                    format!("{} {}", clean_before.trim(), clean_after.trim()).trim(),
                    onsubmit_attr
                )
            } else {
                let param_re =
                    regex::Regex::new(
                        r#"(?is)v-param\s*=\s*["']([^"']+)["']"#,
                    )
                    .unwrap();

                let param = param_re
                    .captures(&all_attrs)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str())
                    .unwrap_or("value");

                let prompt_re =
                    regex::Regex::new(
                        r#"(?is)v-prompt\s*=\s*["']([^"']*)["']"#,
                    )
                    .unwrap();

                let prompt = prompt_re
                    .captures(&all_attrs)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str())
                    .unwrap_or("Enter new value:");

                let prompt_js = js_string_literal(prompt);
                let param_js = js_string_literal(param);

                let onclick = format!(
                    "let v=prompt({});if(v!==null){{fetch({},{{method:'PUT',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{{}:v}})}}).then(async r=>{{if(!r.ok){{let d;try{{d=await r.json()}}catch(_){{d={{}}}};throw new Error(d.details||d.error||'Request failed')}}window.location.href=window.location.pathname+'?status=success&action=updated'}}).catch(e=>{{console.error('[VLO PUT]',e);window.location.href=window.location.pathname+'?status=error&action=error&message='+encodeURIComponent(e.message)}})}}",
                    prompt_js,
                    url_js,
                    param_js
                );

                let onclick_attr = escape_html_attribute(&onclick);

                format!(
                    "<{} {} onclick=\"{}\">",
                    tag,
                    format!("{} {}", clean_before.trim(), clean_after.trim()).trim(),
                    onclick_attr
                )
            }
        })
        .into_owned();

        // ============================================================
    // v-post
    // ============================================================

    let re_post = regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)v-post\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    )
    .unwrap();

    result = re_post
        .replace_all(&result, |caps: &regex::Captures| {
            let tag = caps.get(1).unwrap().as_str();
            let attrs_before = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let url = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let attrs_after = caps.get(4).map(|m| m.as_str()).unwrap_or("");

            let clean_before = strip_vlo_directive_attrs(attrs_before);
            let clean_after = strip_vlo_directive_attrs(attrs_after);

            let url_js = js_string_literal(url);

            if tag.eq_ignore_ascii_case("form") {
            let onsubmit = format!(
                "event.preventDefault();fetch({},{{method:'POST',body:new FormData(event.currentTarget)}}).then(async r=>{{let d;try{{d=await r.json()}}catch(_){{d={{}}}}if(!r.ok){{throw new Error(d.message||d.details||d.error||'Request failed')}}window.location.href=window.location.pathname+'?status=success&action=created&message='+encodeURIComponent(d.message||'Operation successful.')}}).catch(e=>{{console.error('[VLO POST]',e);window.location.href=window.location.pathname+'?status=error&action=error&message='+encodeURIComponent(e.message)}});return false",
                url_js
            );

                let onsubmit_attr = escape_html_attribute(&onsubmit);

                format!(
                    "<{} {} onsubmit=\"{}\">",
                    tag,
                    format!("{} {}", clean_before.trim(), clean_after.trim()).trim(),
                    onsubmit_attr
                )
            } else {
                format!(
                    "<{} {}>",
                    tag,
                    format!("{} {}", clean_before.trim(), clean_after.trim()).trim()
                )
            }
        })
        .into_owned();

    vlo_debug!(
        "🧩 [VLO DIRECTIVES] Resolved HTML: {} chars, {} rows",
        result.len(),
        result.lines().count()
    );

    result
}