use crate::{
    api::{execute_api_sql, load_api_actions, strip_server_block},
    component::{render_components, render_tag},
    database::DB_POOL,
    state::{RenderedPage, STYLE_RE},
    template::{clean_empty_tags, render_control_flow},
};
use axum::{
    extract::{Path as AxumPath, Query},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive},
        Html, IntoResponse, Sse,
    },
};
use futures_util::stream::Stream;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use std::{
    collections::HashMap,
    convert::Infallible,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::broadcast;

pub fn resolve_data_sources(
    source: &str,
    page_context: &HashMap<String, Value>,
) -> String {
    let re = regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)data-source\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    )
    .unwrap();

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
            let next_open = source[cursor..]
                .find(&open)
                .map(|p| cursor + p);

            let next_close = source[cursor..]
                .find(&close)
                .map(|p| cursor + p);

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

        let rendered = evaluate_data_source_block(
            inner,
            &action,
            page_context,
        );

        result.push_str(&format!(
            "<{}{}{}>{}</{}>",
            tag,
            before,
            after,
            rendered,
            tag
        ));

        last_end = close_pos + close.len();
    }

    result.push_str(&source[last_end..]);
    result
}

pub fn fetch_api_data_sync(action: &str) -> Value {
    let action = action.to_string();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(async {
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
        })
    })
    .join()
    .unwrap_or(Value::Array(vec![]))
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

pub fn render_vlo(source: String) -> RenderedPage {
    render_vlo_with_query(source, &HashMap::new())
}

pub fn render_vlo_with_query(
    source: String,
    query: &HashMap<String, String>,
) -> RenderedPage {
    let mut context = RenderedPage::default();

    for (key, value) in query {
        context.insert(
            key,
            Value::String(value.clone()),
        );
    }

    let mut source = strip_server_block(&source);

    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) {
            context.add_style("page", style.as_str());
        }
    }

    source = STYLE_RE.replace_all(&source, "").into_owned();

    source = resolve_data_sources(
        &source,
        &context.template_context,
    );

    for _ in 0..20 {
        let previous = source.clone();

        source = render_tag(
            &source,
            "BaseLayout",
            &mut context,
        );

        source = render_components(
            &source,
            &mut context,
        );

        if source == previous {
            break;
        }
    }

    source = crate::server::resolve_directives(&source);

    source = render_control_flow(
        &source,
        &context.template_context,
    );

    if query.contains_key("status")
        || query.contains_key("action")
    {
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

pub async fn home_handler(
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    render_page(
        "home".to_string(),
        true,
        query,
    )
    .await
}

pub async fn page_handler(
    AxumPath(path): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    render_page(
        path,
        true,
        query,
    )
    .await
}

pub async fn not_found_handler() -> impl IntoResponse {
    render_404(true).await
}

pub async fn render_page(
    path: String,
    dev: bool,
    query: HashMap<String, String>,
) -> impl IntoResponse {
    let page_path = path.clone();

    match tokio::task::spawn_blocking(move || {
        let file = crate::state::get_project_root()
            .join("pages")
            .join(format!("{}.vlo", page_path));

        fs::read_to_string(file).ok().map(|content| {
            let rendered = render_vlo_with_query(
                content,
                &query,
            );

            (
                StatusCode::OK,
                Html(wrap_html(
                    &page_path,
                    &rendered,
                    dev,
                )),
            )
        })
    })
    .await
    {
        Ok(Some(response)) => response.into_response(),
        _ => render_404(dev).await.into_response(),
    }
}

pub async fn render_404(dev: bool) -> impl IntoResponse {
    tokio::task::spawn_blocking(move || {
        let file = crate::state::get_project_root()
            .join("pages")
            .join("404.vlo");

        if let Ok(content) = fs::read_to_string(file) {
            let rendered = render_vlo(content);

            (
                StatusCode::NOT_FOUND,
                Html(wrap_html(
                    "404 - Page Not Found",
                    &rendered,
                    dev,
                )),
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
                Html(wrap_html(
                    "404 - Page Not Found",
                    &rendered,
                    dev,
                )),
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

pub fn wrap_html(
    title: &str,
    rendered: &RenderedPage,
    dev: bool,
) -> String {
    let hmr = if dev {
        r#"<script>
const es = new EventSource("/__vlo_hmr");
es.onmessage = () => location.reload();
window.addEventListener("beforeunload", () => es.close());
</script>"#
    } else {
        ""
    };

    let component_styles = if rendered.styles.is_empty() {
        String::new()
    } else {
        format!(
            "\n<style>\n{}\n</style>",
            rendered.styles.join("\n")
        )
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{}</title>
    <link rel="icon" href="/static/favicon.ico">
    <link rel="stylesheet" href="/static/style.css">{}
</head>
<body>
{}
{}
</body>
</html>"#,
        title,
        component_styles,
        rendered.html,
        hmr
    )
}

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
    let root = crate::state::get_project_root();
    let layouts = root.join("layouts");
    let components = root.join("components");

    let (tx_notify, rx) = std::sync::mpsc::channel();

    let mut watcher =
        RecommendedWatcher::new(
            tx_notify,
            Config::default(),
        )?;

    let paths_to_watch = [
        &pages,
        &public,
        &layouts,
        &components,
    ];

    for path in paths_to_watch {
        if path.exists() {
            watcher.watch(
                path,
                RecursiveMode::Recursive,
            )?;
        }
    }

    for result in rx {
        let Ok(event) = result else {
            continue;
        };

        let relevant = event.paths.iter().any(|path| {
            path.extension()
                .and_then(|e| e.to_str())
                == Some("vlo")
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

                if let Ok(mut cache) =
                    crate::state::TEMPLATE_CACHE.lock()
                {
                    cache.clear();
                }

                let _ = tx.send(());
                println!("⚡ Reload");
            }
        }
    }

    Ok(())
}