use crate::{
    api::{execute_api_sql, load_api_actions, strip_server_block},
    component::{render_components, render_tag},
    database::DB_POOL,
    state::{self, RenderedPage, STYLE_RE},
    template::{
        clean_empty_tags,
        preserve_runtime_data_sources,
        preserve_runtime_query_interpolations,
        render_control_flow,
        render_control_flow_for_build,
        restore_runtime_data_sources,
        restore_runtime_query_interpolations,
    },
};

use axum::{
    extract::{Path as AxumPath, Query},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive},
        Html, IntoResponse, Sse, Response,
    },
};
use futures_util::stream::Stream;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use std::{
    collections::HashMap,
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Instant,
};
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// Shared resources
// ---------------------------------------------------------------------------
static DATA_SOURCE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)data-source\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    )
    .unwrap()
});

static DATA_RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
});

// ---------------------------------------------------------------------------
// Hierarchical layout resolution
// ---------------------------------------------------------------------------
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn resolve_layout_name(page_path: &str) -> String {
    let root = state::get_project_root();
    let layouts_dir = root.join("layouts");
    let page = Path::new(page_path);
    let stem = page
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(page_path);

    let mut candidates: Vec<(PathBuf, String)> = Vec::new();

    // .filter() converts Some("") into None for top-level pages like "home" or "docs"
    let parent = page.parent().filter(|p| !p.as_os_str().is_empty());

    if let Some(parent) = parent {
        let parent_name = parent
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        
        // 1. layouts/{parent}/{stem}Layout.vlo (e.g., layouts/admin/dashboardLayout.vlo)
        candidates.push((layouts_dir.join(parent), stem.to_string()));
        
        // 2. layouts/{parent}Layout.vlo (e.g., layouts/adminLayout.vlo)
        candidates.push((layouts_dir.clone(), parent_name));
    } else {
        // 1. layouts/{stem}/{stem}Layout.vlo (e.g., layouts/docs/DocsLayout.vlo)
        candidates.push((layouts_dir.join(stem), stem.to_string()));
        
        // 2. layouts/{stem}Layout.vlo (e.g., layouts/docsLayout.vlo)
        candidates.push((layouts_dir.clone(), stem.to_string()));
    }

    for (dir, base) in &candidates {
        let variants = [
            format!("{}Layout", base),
            format!("{}Layout", capitalize(base)),
            format!("{}Layout", base.to_ascii_lowercase()),
        ];
        for variant in &variants {
            let path = dir.join(format!("{}.vlo", variant));
            if path.exists() {
                crate::vlo_debug!("🎨 Layout resolved: '{}' -> '{}'", page_path, variant);
                return variant.clone();
            }
        }
    }

    crate::vlo_debug!("🎨 Layout fallback: '{}' -> 'BaseLayout'", page_path);
    "BaseLayout".to_string()
}

/// Resolves the layout for a page and rewrites the source so it uses it.
/// Returns (new_source, layout_name).
fn apply_layout(page_path: &str, source: String) -> (String, String) {
    if page_path.is_empty() {
        return (source, "BaseLayout".to_string());
    }

    let layout_name = resolve_layout_name(page_path);

    if layout_name == "BaseLayout" {
        return (source, layout_name);
    }

    let open_base = "<BaseLayout";
    let close_base = "</BaseLayout>";
    let open_target = format!("<{}", layout_name);
    let close_target = format!("</{}>", layout_name);

    if source.contains(open_base) {
        // Page declared <BaseLayout> — upgrade it to the resolved layout.
        crate::vlo_debug!(
            "🎨 VLO LAYOUT: swapping <BaseLayout> -> <{}>",
            layout_name
        );
        let swapped = source
            .replace(open_base, &open_target)
            .replace(close_base, &close_target);
        (swapped, layout_name)
    } else if source.contains(&open_target) {
        // Page already uses the resolved layout explicitly.
        (source, layout_name)
    } else {
        // No layout tag at all — wrap the whole page.
        crate::vlo_debug!(
            "🎨 VLO LAYOUT: wrapping page in <{}>",
            layout_name
        );
        (
            format!("<{}>{}</{}>", layout_name, source, layout_name),
            layout_name,
        )
    }
}

// ---------------------------------------------------------------------------
// Data-source resolution
// ---------------------------------------------------------------------------
pub fn resolve_data_sources(
    source: &str,
    page_context: &HashMap<String, Value>,
) -> String {
    let re = &*DATA_SOURCE_RE;
    let mut result = String::with_capacity(source.len());
    let mut last_end = 0usize;

    for cap in re.captures_iter(source) {
        let full = cap.get(0).unwrap();
        if full.start() < last_end {
            continue;
        }

        let tag = cap.get(1).unwrap().as_str();
        let before = cap.get(2).unwrap().as_str();
        let action = cap
            .get(3)
            .unwrap()
            .as_str()
            .trim_start_matches("/api/")
            .trim_matches('/')
            .to_string();
        let after = cap.get(4).unwrap().as_str();

        let close = format!("</{}>", tag);
        let open = format!("<{}", tag);
        let mut depth = 1usize;
        let mut cursor = full.end();
        let mut close_start = None;

        while cursor < source.len() {
            let next_open = source[cursor..].find(&open).map(|p| cursor + p);
            let next_close = source[cursor..].find(&close).map(|p| cursor + p);

            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    let after_open = o + open.len();
                    if source
                        .get(after_open..)
                        .map(|v| {
                            v.starts_with('>')
                                || v.starts_with(' ')
                                || v.starts_with('/')
                        })
                        .unwrap_or(false)
                    {
                        depth += 1;
                    }
                    cursor = after_open;
                }
                (_, Some(c)) => {
                    depth -= 1;
                    if depth == 0 {
                        close_start = Some(c);
                        break;
                    }
                    cursor = c + close.len();
                }
                _ => break,
            }
        }

        let Some(close_pos) = close_start else {
            result.push_str(&source[last_end..]);
            return result;
        };

        result.push_str(&source[last_end..full.start()]);
        let inner = &source[full.end()..close_pos];
        let rendered = evaluate_data_source_block(inner, &action, page_context);
        result.push_str(&format!(
            "<{}{}{}>{}</{}>",
            tag, before, after, rendered, tag
        ));
        last_end = close_pos + close.len();
    }

    result.push_str(&source[last_end..]);
    result
}

pub fn fetch_api_data_sync(action: &str) -> Value {
    let action = action.to_string();
    let fetch_logic = async move {
        let actions = load_api_actions().unwrap_or_default();
        if let Some(sql) = actions.get(&action) {
            if let Some(pool) = DB_POOL.get() {
                let params = serde_json::Map::new();
                if let Ok(res) = execute_api_sql(pool, sql, &params).await {
                    if let Some(data) = res.get("data") {
                        return data.clone();
                    }
                }
            }
        }
        Value::Array(vec![])
    };

    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || DATA_RT.block_on(fetch_logic))
            .join()
            .unwrap_or(Value::Array(vec![]))
    } else {
        DATA_RT.block_on(fetch_logic)
    }
}

pub fn evaluate_data_source_block(
    inner: &str,
    action: &str,
    page_context: &HashMap<String, Value>,
) -> String {
    let data = fetch_api_data_sync(action);
    let mut context = page_context.clone();
    let var_name = action
        .trim_start_matches("get_")
        .trim_start_matches("post_")
        .trim_start_matches("put_")
        .trim_start_matches("patch_")
        .trim_start_matches("delete_");
    context.insert(var_name.to_string(), data);
    render_control_flow(inner, &context)
}

// ---------------------------------------------------------------------------
// Rendering entry points
// ---------------------------------------------------------------------------
pub fn render_vlo(source: String) -> RenderedPage {
    render_vlo_with_query(source, &HashMap::new())
}

#[allow(dead_code)]
pub fn render_vlo_for_page(page_path: &str, source: String) -> RenderedPage {
    render_vlo_with_query_at(page_path, source, &HashMap::new())
}

#[allow(dead_code)]
pub fn render_vlo_for_build(source: String) -> RenderedPage {
    render_vlo_for_build_at("", source)
}

pub fn render_vlo_for_build_at(
    page_path: &str,
    source: String,
) -> RenderedPage {
    let mut context = RenderedPage::default();

    // Inject current path for active nav link resolution
    let url_path = if page_path.is_empty() || page_path == "home" || page_path == "index" {
        "/".to_string()
    } else {
        format!("/{}", page_path.trim_matches('/'))
    };
    context.insert("@path", Value::String(url_path));

    let mut source = strip_server_block(&source);

    let (laid_out, layout_name) = apply_layout(page_path, source);
    source = laid_out;

    let (source_with_protected_query, runtime_query_interpolations) =
        preserve_runtime_query_interpolations(&source);
    source = source_with_protected_query;

    let mut runtime_interpolations = Vec::new();
    let interpolation_re =
        regex::Regex::new(r"\{\{\s*([a-zA-Z0-9_@.-]+)\s*\}\}").unwrap();
    let mut protected_source = String::with_capacity(source.len());
    let mut last_end = 0usize;

    for capture in interpolation_re.captures_iter(&source) {
        let full = capture.get(0).unwrap();
        let expression = capture.get(1).unwrap().as_str();
        if expression.contains('.') {
            protected_source.push_str(&source[last_end..full.start()]);
            let index = runtime_interpolations.len();
            runtime_interpolations.push(full.as_str().to_string());
            protected_source.push_str(&format!(
                "__VLO_RUNTIME_DOTTED_INTERPOLATION_{}__",
                index
            ));
            last_end = full.end();
        }
    }
    protected_source.push_str(&source[last_end..]);
    source = protected_source;

    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) {
            context.add_style("page", style.as_str());
        }
    }
    source = STYLE_RE.replace_all(&source, "").into_owned();

    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, &layout_name, &mut context);
        if layout_name != "BaseLayout" {
            source = render_tag(&source, "BaseLayout", &mut context);
        }
        source = render_components(&source, &mut context);
        if source == previous {
            break;
        }
    }

    source = crate::server::resolve_directives(&source);

    let (source_with_runtime_data_sources, runtime_data_sources) =
        preserve_runtime_data_sources(&source);
    source = source_with_runtime_data_sources;

    source = render_control_flow_for_build(&source, &context.template_context);
    source = restore_runtime_data_sources(&source, &runtime_data_sources);

    for (index, interpolation) in runtime_interpolations.iter().enumerate() {
        let marker = format!("__VLO_RUNTIME_DOTTED_INTERPOLATION_{}__", index);
        source = source.replace(&marker, interpolation);
    }
    source = restore_runtime_query_interpolations(&source, &runtime_query_interpolations);

    context.html = clean_empty_tags(&source);
    context
}

pub fn render_vlo_with_query(
    source: String,
    query: &HashMap<String, String>,
) -> RenderedPage {
    render_vlo_with_query_at("", source, query)
}

pub fn render_vlo_with_query_at(
    page_path: &str,
    source: String,
    query: &HashMap<String, String>,
) -> RenderedPage {
    let mut context = RenderedPage::default();
    for (key, value) in query {
        context.insert(key, Value::String(value.clone()));
    }
    // Inject current path so components can use it during rendering
    let url_path = if page_path.is_empty() || page_path == "home" || page_path == "index" {
        "/".to_string()
    } else {
        format!("/{}", page_path.trim_matches('/'))
    };
    context.insert("@path", Value::String(url_path));

    let mut source = strip_server_block(&source);

    let (laid_out, layout_name) = apply_layout(page_path, source);
    source = laid_out;

    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) {
            context.add_style("page", style.as_str());
        }
    }
    source = STYLE_RE.replace_all(&source, "").into_owned();

    source = resolve_data_sources(&source, &context.template_context);

    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, &layout_name, &mut context);
        if layout_name != "BaseLayout" {
            source = render_tag(&source, "BaseLayout", &mut context);
        }
        source = render_components(&source, &mut context);
        if source == previous {
            break;
        }
    }

    source = crate::server::resolve_directives(&source);
    source = render_control_flow(&source, &context.template_context);

    if query.contains_key("status") || query.contains_key("action") {
        source.push_str(
            r#"<script>
(function () {
    if (window.history && window.history.replaceState) {
        window.history.replaceState(
            {},
            document.title,
            window.location.pathname
        );
    }
})();
</script>"#,
        );
    }

    context.html = clean_empty_tags(&source);
    context
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------
pub async fn home_handler(
    axum::Extension(auth): axum::Extension<crate::auth::AuthUser>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    render_page("home".to_string(), query, auth).await
}

pub async fn page_handler(
    AxumPath(path): AxumPath<String>,
    axum::Extension(auth): axum::Extension<crate::auth::AuthUser>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    render_page(path, query, auth).await
}

pub async fn not_found_handler() -> impl IntoResponse {
    render_404().await
}

pub async fn render_page(
    path: String,
    query: HashMap<String, String>,
    auth: crate::auth::AuthUser,
) -> impl IntoResponse {
    let page_path = path.clone();
    match tokio::task::spawn_blocking(move || -> Option<Response> {
        let file = state::get_project_root()
            .join("pages")
            .join(format!("{}.vlo", page_path));

        let content = match fs::read_to_string(&file) {
            Ok(c) => c,
            Err(_) => return None,
        };

        let mut query = query;

        // ─── PAGE GUARD CHECK ───────────────────────────────
        if let Some(guard) = crate::auth::extract_page_guard(&content) {
            let current_url = if page_path == "home" {
                "/".to_string()
            } else {
                format!("/{}", page_path)
            };
            let next = query.get("next").cloned();

            if let Some(response) = crate::auth::check_page_guard(
                &guard,
                &auth.user,
                &current_url,
                next.as_deref(),
            ) {
                return Some(response);
            }
        }
        // ────────────────────────────────────────────────────

        // Inject auth state so templates can use {if logged_in}
        match &auth.user {
            Some(user) => {
                query.insert("logged_in".to_string(), "true".to_string());
                query.insert("user_name".to_string(), user.name.clone());
                query.insert("user_role".to_string(), user.role.clone());
                query.insert("user_email".to_string(), user.email.clone());
            }
            None => {
                query.insert("logged_in".to_string(), String::new());
            }
        }

        let rendered = render_vlo_with_query_at(&page_path, content, &query);
        Some((StatusCode::OK, Html(wrap_html(&page_path, &rendered))).into_response())
    })
    .await
    {
        Ok(Some(response)) => response,
        _ => render_404().await.into_response(),
    }
}

pub async fn render_404() -> impl IntoResponse {
    tokio::task::spawn_blocking(move || {
        let file = state::get_project_root().join("pages").join("404.vlo");
        if let Ok(content) = fs::read_to_string(file) {
            let rendered = render_vlo(content);
            (
                StatusCode::NOT_FOUND,
                Html(wrap_html("404 - Page Not Found", &rendered)),
            )
                .into_response()
        } else {
            let rendered = RenderedPage {
                html: r#"<div class="not-found"><h1>404</h1><p>Page Not Found</p><a href="/">Back to Home</a></div>"#
                    .to_string(),
                ..Default::default()
            };
            (
                StatusCode::NOT_FOUND,
                Html(wrap_html("404 - Page Not Found", &rendered)),
            )
                .into_response()
        }
    })
    .await
    .unwrap_or_else(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("Server Error".to_string()),
        )
            .into_response()
    })
}

pub fn wrap_html(title: &str, rendered: &RenderedPage) -> String {
    let dev = state::app_mode().is_dev();
    let component_styles = if rendered.styles.is_empty() {
        String::new()
    } else {
        format!("\n<style>\n{}\n</style>", rendered.styles.join("\n"))
    };
    let hmr = if dev {
        r#"<script>
const es = new EventSource("/__vlo_hmr");
es.onmessage = () => location.reload();
window.addEventListener("beforeunload", () => es.close());
</script>"#
    } else {
        ""
    };

    let mut html = rendered.html.clone();
    html = html.replace("{{title}}", title);
    if !component_styles.is_empty() {
        html = html.replacen("</head>", &format!("{}\n</head>", component_styles), 1);
    }
    if !hmr.is_empty() {
        html = html.replacen("</body>", &format!("{}\n</body>", hmr), 1);
    }
    html
}

// ---------------------------------------------------------------------------
// HMR & file watching
// ---------------------------------------------------------------------------
pub async fn hmr_handler(
    tx: broadcast::Sender<()>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = tx.subscribe();
    let stream = async_stream::stream! {
        while rx.recv().await.is_ok() {
            yield Ok(Event::default().data("reload"));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub fn watch_files(
    pages: PathBuf,
    public: PathBuf,
    tx: broadcast::Sender<()>,
    last: Arc<Mutex<Instant>>,
) -> notify::Result<()> {
    let root = state::get_project_root();
    let layouts = root.join("layouts");
    let components = root.join("components");
    let (tx_notify, rx) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx_notify, Config::default())?;
    let paths_to_watch = [&pages, &public, &layouts, &components];
    for path in paths_to_watch {
        if path.exists() {
            watcher.watch(path, RecursiveMode::Recursive)?;
        }
    }
    for result in rx {
        let Ok(event) = result else { continue; };
        let relevant = event.paths.iter().any(|path| {
            path.extension().and_then(|e| e.to_str()) == Some("vlo")
                || path.starts_with(&public)
                || path.starts_with(&layouts)
                || path.starts_with(&components)
        });
        if !relevant {
            continue;
        }
        if let Ok(mut timestamp) = last.try_lock() {
            if timestamp.elapsed().as_millis() > 200 {
                *timestamp = Instant::now();
                if let Ok(mut cache) = state::TEMPLATE_CACHE.lock() {
                    cache.clear();
                }
                let _ = tx.send(());
                println!("⚡ Reload");
            }
        }
    }
    Ok(())
}