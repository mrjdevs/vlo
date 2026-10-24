// ─────────────────────────────────────────────────────────────
// router.rs — VLO page renderer + HTTP handlers
// Optimized: memoized layout resolution, cached flash detection,
// single-pass style extraction, Cow-based interpolation preservation,
// fast-path for static pages. No new dependencies. All features preserved.
// ─────────────────────────────────────────────────────────────
use crate::{
    api::{execute_api_sql, load_api_actions, strip_server_block},
    component::{render_components, render_tag},
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
    sync::{Arc, LazyLock, Mutex, OnceLock},
    time::Instant,
};
use tokio::sync::broadcast;

// ─────────────────────────────────────────────────────────────
// Shared resources
// ─────────────────────────────────────────────────────────────
static DATA_SOURCE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)data-source\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    )
    .unwrap()
});

/// Only matches `{{ a.b }}` (dotted identifiers) — same set the original
/// `render_vlo_for_build_at` protected with its inline regex.
static DOTTED_INTERP_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\{\{\s*([a-zA-Z0-9_@.-]*\.[a-zA-Z0-9_@.-]+)\s*\}\}").unwrap()
});

static DATA_RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
});

// ─────────────────────────────────────────────────────────────
// Layout resolution (memoized)
// ─────────────────────────────────────────────────────────────
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Memoized page_path → layout name.
static LAYOUT_CACHE: LazyLock<Mutex<HashMap<String, Arc<str>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn clear_layout_cache() {
    if let Ok(mut cache) = LAYOUT_CACHE.lock() {
        cache.clear();
    }
}

fn resolve_layout_name(page_path: &str) -> Arc<str> {
    if let Ok(cache) = LAYOUT_CACHE.lock() {
        if let Some(hit) = cache.get(page_path) {
            return Arc::clone(hit);
        }
    }

    let resolved = compute_layout_name(page_path);
    let arc: Arc<str> = Arc::from(resolved.as_str());
    if let Ok(mut cache) = LAYOUT_CACHE.lock() {
        cache.insert(page_path.to_string(), Arc::clone(&arc));
    }
    arc
}

fn compute_layout_name(page_path: &str) -> String {
    let root = state::get_project_root();
    let layouts_dir = root.join("layouts");
    let page = Path::new(page_path);
    let stem = page.file_stem().and_then(|s| s.to_str()).unwrap_or(page_path);

    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    let parent = page.parent().filter(|p| !p.as_os_str().is_empty());

    if let Some(parent) = parent {
        let parent_name = parent.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
        candidates.push((layouts_dir.join(parent), stem.to_string()));
        candidates.push((layouts_dir.clone(), parent_name));
    } else {
        candidates.push((layouts_dir.join(stem), stem.to_string()));
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
                return variant.clone();
            }
        }
    }
    "BaseLayout".to_string()
}

fn apply_layout(page_path: &str, source: String) -> (String, Arc<str>) {
    if page_path.is_empty() {
        return (source, Arc::from("BaseLayout"));
    }
    let layout_name = resolve_layout_name(page_path);
    if &*layout_name == "BaseLayout" {
        return (source, layout_name);
    }

    // Fast path: source already uses the resolved name.
    let open_target = format!("<{}", layout_name);
    if source.contains(&open_target) {
        return (source, layout_name);
    }

    let open_base = "<BaseLayout";
    let close_base = "</BaseLayout>";
    let close_target = format!("</{}>", layout_name);

    if source.contains(open_base) {
        let swapped = source
            .replace(open_base, &open_target)
            .replace(close_base, &close_target);
        (swapped, layout_name)
    } else {
        (
            format!("<{}>{}</{}>", layout_name, source, layout_name),
            layout_name,
        )
    }
}

// ─────────────────────────────────────────────────────────────
// Data-source resolution
// ─────────────────────────────────────────────────────────────
pub fn resolve_data_sources(
    source: &str,
    page_context: &HashMap<String, Value>,
) -> (String, HashMap<String, Value>) {
    // Fast path: no markers, nothing to do.
    if !source.contains("data-source") {
        return (source.to_string(), HashMap::new());
    }

    let re = &*DATA_SOURCE_RE;
    let mut result = String::with_capacity(source.len());
    let mut last_end = 0usize;
    let mut all_computed: HashMap<String, Value> = HashMap::new();

    for cap in re.captures_iter(source) {
        let full = cap.get(0).unwrap();
        if full.start() < last_end {
            continue;
        }

        let tag = cap.get(1).unwrap().as_str();
        let before = cap.get(2).unwrap().as_str();
        let after = cap.get(4).unwrap().as_str();

        // Substitute {{var}} before parsing. Shadowed so downstream code
        // (action_path, inline_params) needs no changes.
        let raw_url = cap.get(3).unwrap().as_str();
        let raw_url = crate::template::render_interpolations(raw_url, page_context);
        let (action_path, inline_params) = split_action_url(&raw_url);

        let variable_name = action_path
            .split('/')
            .last()
            .unwrap_or(&action_path)
            .replace('-', "_");

        let close = format!("</{}>", tag);
        let open = format!("<{}", tag);
        let mut depth = 1usize;
        let mut cursor = full.end();
        let mut close_start = None;

        // Match nested same-tag opens/closes.
        while cursor < source.len() {
            let next_open = source[cursor..].find(&open).map(|p| cursor + p);
            let next_close = source[cursor..].find(&close).map(|p| cursor + p);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    let after_open = o + open.len();
                    // Only count as nesting if next char is a real boundary.
                    if source
                        .get(after_open..)
                        .map(|v| v.starts_with('>') || v.starts_with(' ') || v.starts_with('/'))
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
            return (result, all_computed);
        };

        result.push_str(&source[last_end..full.start()]);
        let inner = &source[full.end()..close_pos];

        // ── Build a per-block context ─────────────────────────
        // Page-level params (from the URL query) are the base.
        // Inline `?k=v` from the data-source URL override them.
        let block_context = if inline_params.is_empty() {
            page_context.clone()
        } else {
            let mut ctx = page_context.clone();
            for (k, v) in &inline_params {
                ctx.insert(k.clone(), string_to_value(v));
            }
            ctx
        };

        let (rendered, computed) =
            evaluate_data_source_block(inner, &action_path, &variable_name, &block_context);

        for (key, value) in computed {
            all_computed.insert(key, value);
        }

        // Reconstruct open tag with correct spacing.
        let trimmed_before = before.trim();
        let trimmed_after = after.trim();
        let open_tag = match (trimmed_before.is_empty(), trimmed_after.is_empty()) {
            (true, true) => format!("<{}>", tag),
            (false, true) => format!("<{} {}>", tag, trimmed_before),
            (true, false) => format!("<{} {}>", tag, trimmed_after),
            (false, false) => format!("<{} {} {}>", tag, trimmed_before, trimmed_after),
        };
        result.push_str(&open_tag);
        result.push_str(&rendered);
        result.push_str("</");
        result.push_str(tag);
        result.push('>');

        last_end = close_pos + close.len();
    }

    result.push_str(&source[last_end..]);
    (result, all_computed)
}

/// Split `/api/product?id=240` into `("product", {"id": "240"})`.
/// Strips the `/api/` prefix, splits on `?`, URL-decodes each inline param.
fn split_action_url(raw: &str) -> (String, HashMap<String, String>) {
    let stripped = raw.trim_start_matches("/api/").trim_matches('/');

    let (path_part, query_part) = match stripped.split_once('?') {
        Some((p, q)) => (p, q),
        None => (stripped, ""),
    };

    let mut params = HashMap::new();
    if !query_part.is_empty() {
        for pair in query_part.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = urlencoding::decode(v)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| v.to_string());
            params.insert(k.to_string(), v);
        }
    }

    (path_part.to_string(), params)
}

/// Coerce a query-string value to a JSON value.
/// Numbers become Number, "true"/"false" become Bool, everything else stays String.
#[inline]
fn string_to_value(value: &str) -> Value {
    if value.is_empty() {
        return Value::String(String::new());
    }
    if value.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(n) = value.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(n) = value.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Value::Number(num);
        }
    }
    Value::String(value.to_string())
}
pub fn fetch_api_data_sync(action: &str, params: &HashMap<String, Value>) -> Value {
    let action = action.to_string();
    let params = params.clone();
    let fetch_logic = async move {
        let actions = load_api_actions().unwrap_or_default();
        let Some(action_def) = actions.get(&action) else {
            return serde_json::json!({ "data": [], "success": false });
        };
        let sql_template = action_def.sql().to_string();

        let Ok(pool) = crate::database::get_pool() else {
            return serde_json::json!({ "data": [], "success": false });
        };

        // Build SQL parameters with pagination.
        let mut sql_params = serde_json::Map::new();
        for (key, value) in &params {
            sql_params.insert(key.clone(), value.clone());
        }
        if !sql_params.contains_key("limit") {
            sql_params.insert("limit".to_string(), Value::Number(20.into()));
        }
        if !sql_params.contains_key("page") {
            sql_params.insert("page".to_string(), Value::Number(1.into()));
        }
        if let (Some(page_val), Some(limit_val)) = (sql_params.get("page"), sql_params.get("limit")) {
            if let (Some(p), Some(l)) = (page_val.as_i64(), limit_val.as_i64()) {
                let safe_page = p.max(1);
                let safe_limit = l.max(1).min(100);
                let offset = (safe_page - 1) * safe_limit;
                sql_params.insert("offset".to_string(), Value::Number(offset.into()));
                sql_params.insert("page".to_string(), Value::Number(safe_page.into()));
                sql_params.insert("limit".to_string(), Value::Number(safe_limit.into()));
            }
        }

        let mut sql = sql_template.clone();
        crate::vlo_debug!("🔍 SORT/SEARCH [{}]: Original SQL = {}", action, sql);

        // Search filter injection.
        if let Some(search) = params.get("search").and_then(|v| v.as_str()) {
            if !search.trim().is_empty() {
                let safe_search = search.replace('\'', "''").replace('%', "\\%").replace('_', "\\_");
                let columns_str = params
                    .get("_columns")
                    .and_then(|v| v.as_str())
                    .unwrap_or("title,description");
                let column_list: Vec<&str> = columns_str
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                    .collect();
                if !column_list.is_empty() {
                    let like_parts: Vec<String> = column_list
                        .iter()
                        .map(|col| format!("{} LIKE '%{}%'", col, safe_search))
                        .collect();
                    let where_clause = format!(" WHERE ({})", like_parts.join(" OR "));
                    sql = inject_clause_before(&sql, &where_clause, &[" ORDER BY ", " LIMIT "]);
                }
            }
        }

        // Sort injection.
        if let Some(sort_col) = params.get("sort").and_then(|v| v.as_str()) {
            if !sort_col.trim().is_empty()
                && sort_col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                let order = params.get("order").and_then(|v| v.as_str()).unwrap_or("asc");
                let safe_order = if order.eq_ignore_ascii_case("desc") { "DESC" } else { "ASC" };
                let order_clause = format!(" ORDER BY {} {}", sort_col, safe_order);
                sql = replace_or_append_clause(&sql, &order_clause, " ORDER BY ", " LIMIT ");
            }
        }

        let Ok(mut res) = execute_api_sql(pool, &sql, &sql_params).await else {
            return serde_json::json!({ "data": [], "success": false });
        };

        // Pagination metadata for get_* actions.
        if action.starts_with("get_") {
            let count_sql = crate::api::generate_count_sql(&sql);
            if !count_sql.is_empty() {
                if let Ok(count_res) = execute_api_sql(pool, &count_sql, &sql_params).await {
                    if let Some(total) = count_res
                        .get("data")
                        .and_then(|d| d.as_array())
                        .and_then(|rows| rows.first())
                        .and_then(|row| row.get("total"))
                        .and_then(|v| v.as_i64())
                    {
                        let page = sql_params.get("page").and_then(|v| v.as_i64()).unwrap_or(1);
                        let limit = sql_params.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
                        let total_pages = (total as f64 / limit as f64).ceil() as i64;
                        let pagination = serde_json::json!({
                            "total": total, "page": page, "limit": limit,
                            "total_pages": total_pages,
                            "has_next": page < total_pages, "has_prev": page > 1
                        });
                        if let Some(obj) = res.as_object_mut() {
                            obj.insert("pagination".to_string(), pagination);
                        }
                    }
                }
            }
        }
        res
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(fetch_logic))
    } else {
        DATA_RT.block_on(fetch_logic)
    }
}

/// Insert `clause` before the first matching marker (case-insensitive),
/// otherwise append.
#[inline]
fn inject_clause_before(sql: &str, clause: &str, markers: &[&str]) -> String {
    let upper = sql.to_uppercase();
    for marker in markers {
        if let Some(pos) = upper.find(marker) {
            let mut out = String::with_capacity(sql.len() + clause.len());
            out.push_str(&sql[..pos]);
            out.push_str(clause);
            out.push_str(&sql[pos..]);
            return out;
        }
    }
    format!("{}{}", sql.trim_end_matches(';'), clause)
}

/// Replace an existing `ORDER BY ...` block, or insert before LIMIT,
/// or append.
#[inline]
fn replace_or_append_clause(sql: &str, clause: &str, primary: &str, secondary: &str) -> String {
    let upper = sql.to_uppercase();
    if let Some(pos) = upper.find(primary) {
        let after = &sql[pos..];
        let after_upper = after.to_uppercase();
        let end_offset = after_upper
            .find(secondary)
            .or_else(|| after_upper.find(';'))
            .unwrap_or(after.len());
        let mut out = String::with_capacity(sql.len() + clause.len());
        out.push_str(&sql[..pos]);
        out.push_str(clause);
        out.push_str(&after[end_offset..]);
        out
    } else if let Some(pos) = upper.find(secondary) {
        let mut out = String::with_capacity(sql.len() + clause.len());
        out.push_str(&sql[..pos]);
        out.push_str(clause);
        out.push_str(&sql[pos..]);
        out
    } else {
        format!("{}{}", sql.trim_end_matches(';'), clause)
    }
}

pub fn evaluate_data_source_block(
    inner: &str,
    action_path: &str,
    variable_name: &str,
    page_context: &HashMap<String, Value>,
) -> (String, HashMap<String, Value>) {
        // ─── TEMP DEBUG ───
    eprintln!(
        "🔍 [DS] action_path={:?} variable_name={:?} context_keys={:?}",
        action_path,
        variable_name,
        page_context.keys().collect::<Vec<_>>()
    );
    // ──────────────────
    // Infer action name if given a RESTful path.
    let action_name = if action_path.contains('/')
        || action_path.starts_with("get_")
        || action_path.starts_with("post_")
        || action_path.starts_with("put_")
        || action_path.starts_with("delete_")
    {
        action_path.to_string()
    } else {
        format!("get_{}", action_path)
    };

    let api_response = fetch_api_data_sync(&action_name, page_context);
    let mut context = page_context.clone();
    let mut computed: HashMap<String, Value> = HashMap::new();

    let data = api_response.get("data").cloned().unwrap_or(Value::Array(vec![]));
    context.insert(variable_name.to_string(), data.clone());
    computed.insert(variable_name.to_string(), data.clone());

    // Also insert without "get_" prefix for cleaner templates.
    if variable_name.starts_with("get_") {
        let clean_name = variable_name.trim_start_matches("get_").to_string();
        if !clean_name.is_empty() {
            context.insert(clean_name.clone(), data.clone());
            computed.insert(clean_name, data);
        }
    }

    // Hoist pagination fields into top-level context.
    if let Some(pagination) = api_response.get("pagination").and_then(|p| p.as_object()) {
        for (key, value) in pagination {
            context.insert(key.clone(), value.clone());
            computed.insert(key.clone(), value.clone());
        }
    }

    let rendered = render_control_flow(inner, &context);
    (rendered, computed)
}

// ─────────────────────────────────────────────────────────────
// Style extraction (single pass)
// ─────────────────────────────────────────────────────────────
/// Extract `<style>` blocks into the RenderedPage, returning the source
/// with them removed. Single scan instead of captures_iter + replace_all.
fn extract_styles(source: String, context: &mut RenderedPage) -> String {
    if !source.contains("<style") {
        return source;
    }

    let mut out = String::with_capacity(source.len());
    let mut last_end = 0usize;
    let mut any = false;

    for captures in STYLE_RE.captures_iter(&source) {
        let full = captures.get(0).unwrap();
        if let Some(style) = captures.get(1) {
            context.add_style("page", style.as_str());
            any = true;
        }
        out.push_str(&source[last_end..full.start()]);
        last_end = full.end();
    }
    if !any {
        return source;
    }
    out.push_str(&source[last_end..]);
    out
}

// ─────────────────────────────────────────────────────────────
// Runtime interpolation preservation (dotted identifiers)
// ─────────────────────────────────────────────────────────────
fn preserve_dotted_interpolations(source: &str) -> (String, Vec<String>) {
    if !source.contains("{{") {
        return (source.to_string(), Vec::new());
    }

    let mut runtime_interpolations: Vec<String> = Vec::new();
    let mut protected = String::with_capacity(source.len());
    let mut last_end = 0usize;

    for capture in DOTTED_INTERP_RE.captures_iter(source) {
        let full = capture.get(0).unwrap();
        protected.push_str(&source[last_end..full.start()]);
        let index = runtime_interpolations.len();
        runtime_interpolations.push(full.as_str().to_string());
        protected.push_str("__VLO_RUNTIME_DOTTED_INTERPOLATION_");
        protected.push_str(&index.to_string());
        protected.push_str("__");
        last_end = full.end();
    }
    if runtime_interpolations.is_empty() {
        return (source.to_string(), runtime_interpolations);
    }
    protected.push_str(&source[last_end..]);
    (protected, runtime_interpolations)
}

fn restore_dotted_interpolations(source: String, interpolations: &[String]) -> String {
    if interpolations.is_empty() {
        return source;
    }
    let mut out = source;
    for (index, interpolation) in interpolations.iter().enumerate() {
        let marker = format!("__VLO_RUNTIME_DOTTED_INTERPOLATION_{}__", index);
        if out.contains(&marker) {
            out = out.replace(&marker, interpolation);
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────
// Rendering entry points
// ─────────────────────────────────────────────────────────────
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

pub fn render_vlo_for_build_at(page_path: &str, source: String) -> RenderedPage {
    let mut context = RenderedPage::default();
    let url_path = if page_path.is_empty() || page_path == "index" {
        "/".to_string()
    } else {
        format!("/{}", page_path.trim_matches('/'))
    };
    context.insert("@path", Value::String(url_path));

    let source = strip_server_block(&source);
    let (source, layout_name) = apply_layout(page_path, source);

    // Preserve runtime dotted interpolations and query interpolations.
    let (source, runtime_query_interpolations) = preserve_runtime_query_interpolations(&source);
    let (source, runtime_interpolations) = preserve_dotted_interpolations(&source);
    let mut source = source;

    // Extract styles once.
    source = extract_styles(source, &mut context);

    // Component/layout render loop.
    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, &layout_name, &mut context);
        if &*layout_name != "BaseLayout" {
            source = render_tag(&source, "BaseLayout", &mut context);
        }
        source = render_components(&source, &mut context);
        if source == previous {
            break;
        }
    }

    // Preserve runtime data sources, run control flow, restore.
    let (mut source, runtime_data_sources) = preserve_runtime_data_sources(&source);
    source = render_control_flow_for_build(&source, &context.template_context);
    source = restore_runtime_data_sources(&source, &runtime_data_sources);
    source = restore_dotted_interpolations(source, &runtime_interpolations);
    source = restore_runtime_query_interpolations(&source, &runtime_query_interpolations);

    // Directives + conditional scripts (template engine fully done).
    source = crate::server::resolve_directives(&source);
    source = crate::server::inject_conditional_scripts(&source);

    context.html = clean_empty_tags(&source);
    context
}

pub fn render_vlo_with_query(source: String, query: &HashMap<String, String>) -> RenderedPage {
    render_vlo_with_query_at("", source, query)
}

pub fn render_vlo_with_query_at(
    page_path: &str,
    source: String,
    query: &HashMap<String, String>,
) -> RenderedPage {
    let mut context = RenderedPage::default();

    // Query → template_context.
    for (key, value) in query {
        if key == "_flash_messages" {
            continue;
        }
        if let Ok(n) = value.parse::<i64>() {
            context.insert(key, Value::Number(n.into()));
        } else {
            context.insert(key, Value::String(value.clone()));
        }
    }

    // Flash injection.
    if let Some(flash_str) = query.get("_flash_messages") {
        if let Ok(flashes) = serde_json::from_str::<Vec<Value>>(flash_str) {
            context.insert("flash_messages", Value::Array(flashes.clone()));
            if let Some(first) = flashes.first().and_then(|f| f.as_object()) {
                for (key, value) in first {
                    context.insert(&format!("flash_{}", key), value.clone());
                }
                for key in ["variant", "icon", "title", "description"] {
                    if let Some(v) = first.get(key) {
                        context.insert(key, v.clone());
                    }
                }
                let desc_len = first
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                let duration = if desc_len < 30 {
                    "short"
                } else if desc_len < 80 {
                    "medium"
                } else {
                    "long"
                };
                context.insert("duration", Value::String(duration.to_string()));
            }
        }
    }

    let url_path = if page_path.is_empty() || page_path == "index" {
        "/".to_string()
    } else {
        format!("/{}", page_path.trim_matches('/'))
    };
    context.insert("@path", Value::String(url_path));

    let source = strip_server_block(&source);
    let (source, layout_name) = apply_layout(page_path, source);
    let mut source = source;

    // Extract styles once.
    source = extract_styles(source, &mut context);

    // STEP 1: Data-source resolution (fast-path if no markers).
    let (resolved_source, computed_vars) = resolve_data_sources(&source, &context.template_context);
    source = resolved_source;
    if !computed_vars.is_empty() {
        for (key, value) in computed_vars {
            context.insert(&key, value);
        }
    }

    // STEP 2: Layout + component loop.
    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, &layout_name, &mut context);
        if &*layout_name != "BaseLayout" {
            source = render_tag(&source, "BaseLayout", &mut context);
        }
        source = render_components(&source, &mut context);
        if source == previous {
            break;
        }
    }

    // STEP 3: Control flow ({if}, {for}).
    source = render_control_flow(&source, &context.template_context);

    // STEP 4: Second component pass (renders tags generated by control flow).
    source = render_components(&source, &mut context);

    // STEP 5: Directives + conditional scripts.
    source = crate::server::resolve_directives(&source);
    source = crate::server::inject_conditional_scripts(&source);

    // URL cleanup on status/action query.
    if query.contains_key("status") || query.contains_key("action") {
        source.push_str(CLEAN_URL_SCRIPT);
    }

    context.html = clean_empty_tags(&source);
    context
}

const CLEAN_URL_SCRIPT: &str = r#"<script>
(function () { if (window.history && window.history.replaceState) { window.history.replaceState({}, document.title, window.location.pathname); } })();
</script>"#;

// ─────────────────────────────────────────────────────────────
// HTTP handlers
// ─────────────────────────────────────────────────────────────
pub async fn home_handler(
    axum::Extension(auth): axum::Extension<crate::auth::AuthUser>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let flash_encoded = extract_flash_cookie(&headers);
    render_page("index".to_string(), query, auth, flash_encoded).await
}

pub async fn page_handler(
    AxumPath(path): AxumPath<String>,
    axum::Extension(auth): axum::Extension<crate::auth::AuthUser>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let flash_encoded = extract_flash_cookie(&headers);
    render_page(path, query, auth, flash_encoded).await
}

#[inline]
fn extract_flash_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| crate::auth::parse_cookie_header(h, crate::auth::FLASH_COOKIE))
}

pub async fn not_found_handler() -> impl IntoResponse {
    render_404().await
}

/// Memoized flash detection per page.
static LAYOUT_FLASH_CACHE: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn clear_layout_flash_cache() {
    if let Ok(mut cache) = LAYOUT_FLASH_CACHE.lock() {
        cache.clear();
    }
}

fn layout_uses_flash(page_path: &str) -> bool {
    if let Ok(cache) = LAYOUT_FLASH_CACHE.lock() {
        if let Some(hit) = cache.get(page_path) {
            return *hit;
        }
    }

    let result = compute_layout_uses_flash(page_path);
    if let Ok(mut cache) = LAYOUT_FLASH_CACHE.lock() {
        cache.insert(page_path.to_string(), result);
    }
    result
}

fn compute_layout_uses_flash(page_path: &str) -> bool {
    let root = state::get_project_root();
    let layouts_dir = root.join("layouts");
    let page = Path::new(page_path);
    let stem = page.file_stem().and_then(|s| s.to_str()).unwrap_or(page_path);
    let parent = page.parent().filter(|p| !p.as_os_str().is_empty());

    let mut candidates: Vec<PathBuf> = Vec::with_capacity(4);
    if let Some(parent) = parent {
        let parent_name = parent.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
        candidates.push(layouts_dir.join(parent).join(format!("{}Layout.vlo", stem)));
        candidates.push(layouts_dir.join(parent).join(format!("{}Layout.vlo", parent_name)));
    } else {
        candidates.push(layouts_dir.join(stem).join(format!("{}Layout.vlo", stem)));
        candidates.push(layouts_dir.join(format!("{}Layout.vlo", stem)));
    }
    candidates.push(layouts_dir.join("BaseLayout.vlo"));

    for path in candidates {
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                return content.contains("flash_messages") || content.contains("<Toast");
            }
            return false;
        }
    }
    false
}

pub async fn render_page(
    path: String,
    query: HashMap<String, String>,
    auth: crate::auth::AuthUser,
    flash_encoded: Option<String>,
) -> impl IntoResponse {
    let page_path = path.clone();

    let result = tokio::task::spawn_blocking(move || -> Option<Response> {
    let root = state::get_project_root();
    let pages_dir = root.join("pages");

    // 1. Try exact:    pages/admin.vlo
    // 2. Fallback:     pages/admin/index.vlo
    // 3. Cleanup:      strip trailing slash from "admin/"
    let clean_path = page_path.trim_end_matches('/');
    let mut file = pages_dir.join(format!("{}.vlo", clean_path));
    if !file.exists() {
        let index_file = pages_dir.join(clean_path).join("index.vlo");
        if index_file.exists() {
            file = index_file;
        }
    }
    let content = fs::read_to_string(&file).ok()?;
        let mut query = query;

        // Page guard check.
        if let Some(guard) = crate::auth::extract_page_guard(&content) {
            if crate::features::is_enabled(crate::features::Feature::Auth) {
                let current_url = if page_path == "index" {
                    "/".to_string()
                } else {
                    format!("/{}", page_path)
                };
                let next = query.get("next").cloned();
                if let Some(response) =
                    crate::auth::check_page_guard(&guard, &auth.user, &current_url, next.as_deref())
                {
                    return Some(response);
                }
            } else if guard.auth {
                crate::vlo_debug!(
                    "⚠️ Page {} requires auth, but Auth feature is disabled",
                    page_path
                );
                return Some(
                    (StatusCode::NOT_FOUND, Html("404 - Page Not Found")).into_response(),
                );
            }
        }

        // Auth + CSRF injection.
        if crate::features::is_enabled(crate::features::Feature::Auth) {
            let cfg = crate::auth::auth_config();
            query.insert("auth_identifier_field".to_string(), cfg.identifier_field.clone());
            query.insert("auth_password_field".to_string(), cfg.password_field.clone());

            match &auth.user {
                Some(user) => {
                    query.insert("logged_in".to_string(), "true".to_string());
                    // 🔥 FIX: Convert the i64 to a String
                    query.insert("user_id".to_string(), user.id.to_string()); 
                    query.insert("user_name".to_string(), user.name.clone());
                    query.insert("user_role".to_string(), user.role.clone());
                    query.insert("user_email".to_string(), user.email.clone());
                }
                None => {
                    query.insert("logged_in".to_string(), String::new());
                }
            }

            query.insert(
                "csrf_token".to_string(),
                auth.csrf_token.clone().unwrap_or_default(),
            );
        } else {
            query.insert("logged_in".to_string(), String::new());
            query.insert("csrf_token".to_string(), String::new());
        }

        // Pagination.
        let page: i64 = query.get("page").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
        let limit: i64 = query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20).max(1).min(100);
        let offset = (page - 1) * limit;
        query.insert("page".to_string(), page.to_string());
        query.insert("limit".to_string(), limit.to_string());
        query.insert("offset".to_string(), offset.to_string());
        query.insert("prev_page".to_string(), (page - 1).max(1).to_string());
        query.insert("next_page".to_string(), (page + 1).to_string());

        // Search & sort defaults.
        if !query.contains_key("order") {
            query.insert("order".to_string(), "asc".to_string());
        }

        // Flash detection.
        let page_handles_flash = content.contains("flash_messages") || layout_uses_flash(&page_path);

        let flash_consumed = if let Some(encoded) = &flash_encoded {
            if let Some(flash) = crate::auth::decode_flash(encoded) {
                query.insert(
                    "_flash_messages".to_string(),
                    serde_json::to_string(&vec![flash]).unwrap_or_default(),
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        let rendered = render_vlo_with_query_at(&page_path, content, &query);

        let auto_flash = !page_handles_flash;
        let mut response =
            (StatusCode::OK, Html(wrap_html(&page_path, &rendered, auto_flash))).into_response();

        if flash_consumed {
            let expire_cookie = crate::auth::expire_flash_cookie();
            if let Ok(val) = axum::http::HeaderValue::from_str(&expire_cookie) {
                response
                    .headers_mut()
                    .append(axum::http::header::SET_COOKIE, val);
            }
        }

        Some(response)
    })
    .await;

    match result {
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
                Html(wrap_html("404 - Page Not Found", &rendered, true)),
            )
                .into_response()
        } else {
            let rendered = RenderedPage {
                html: r#"<div class="not-found"><h1>404</h1><p>Page Not Found</p><a href="/">Back to Home</a></div>"#.to_string(),
                ..Default::default()
            };
            (
                StatusCode::NOT_FOUND,
                Html(wrap_html("404 - Page Not Found", &rendered, true)),
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

pub async fn page_handler_any(
    uri: axum::http::Uri,
    axum::Extension(auth): axum::Extension<crate::auth::AuthUser>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // Strip leading slash, decode
    let raw = uri.path().trim_start_matches('/');
    let path = urlencoding::decode(raw)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| raw.to_string());

    // 🔒 SECURITY: block path traversal
    if path.is_empty()
        || path.contains("..")
        || path.contains('\\')
        || path.contains('\0')
    {
        return not_found_handler().await.into_response();
    }

    let flash_encoded = extract_flash_cookie(&headers);
    render_page(path, query, auth, flash_encoded).await.into_response()
}

pub fn wrap_html(title: &str, rendered: &RenderedPage, auto_flash: bool) -> String {
    let dev = state::app_mode().is_dev();

    // ─── 1. STYLES ─────────────────────────────────────────
    let mut all_css = String::new();
    if !rendered.styles.is_empty() {
        all_css.push_str(&rendered.styles.join("\n"));
    }
    let all_modules = crate::modules::get_modules();
    for module in &all_modules {
        if rendered.used_modules.contains(&module.manifest.name) && !module.styles.is_empty() {
            all_css.push_str(&module.styles.join("\n"));
            all_css.push('\n');
        }
    }
    let style_block = if all_css.trim().is_empty() {
        String::new()
    } else {
        let css = if dev { all_css } else { crate::utils::minify_css(all_css.trim()) };
        format!("\n<style>\n{}\n</style>", css)
    };

    // ─── 2. MODULE SCRIPTS ─────────────────────────────────
    let mut all_js = String::new();
    for module in &all_modules {
        if rendered.used_modules.contains(&module.manifest.name) && !module.scripts.is_empty() {
            all_js.push_str(&module.scripts.join("\n"));
            all_js.push('\n');
        }
    }
    let script_block = if all_js.trim().is_empty() {
        String::new()
    } else {
        format!("\n<script>\n{}\n</script>", all_js.trim())
    };

    // ─── HMR ───────────────────────────────────────────────
    let hmr = if dev {
        r#"<script>(requestIdleCallback || setTimeout)(function() { const es = new EventSource("/__vlo_hmr"); es.onmessage = function() { location.reload(); }; window.addEventListener("beforeunload", function() { es.close(); }); }, { timeout: 100 });</script>"#
    } else {
        ""
    };

    // ─── CSRF ──────────────────────────────────────────────
    let csrf_meta = if dev {
        if let Some(csrf) = rendered.template_context.get("csrf_token") {
            if let Some(token) = csrf.as_str() {
                if !token.is_empty() {
                    format!(r#"<meta name="csrf-token" content="{}">"#, token)
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        r#"<meta name="csrf-token" content="__VLO_CSRF_PLACEHOLDER__">"#.to_string()
    };

    // ─── FLASH ─────────────────────────────────────────────
    // 🔥 FIX: Removed the hardcoded top-right toast generation.
    // We now rely solely on the placeholder, allowing the <Toast /> 
    // component or serve_build_page to handle rendering cleanly.
    let flash_html = if auto_flash {
        r#"<div id="__VLO_FLASH_PLACEHOLDER__"></div>"#.to_string()
    } else {
        String::new()
    };

    // ─── ACTIVE NAV (build mode) ───────────────────────────
    let active_link_js = if !dev {
        r#"<script id="__VLO_NAV_ACTIVE__">(function(){var p=window.location.pathname;document.querySelectorAll('a[href]').forEach(function(a){var h=a.getAttribute('href');if(!h||h.startsWith('http')||h.startsWith('#')||h.startsWith('javascript'))return;if(h==='/'&&p==='/'){a.classList.add('active')}else if(h!=='/'&&p.startsWith(h)){a.classList.add('active')}})})()</script>"#
    } else {
        ""
    };

    let mut html = rendered.html.clone();
    html = html.replace("{{title}}", title);

    // ─── FORM VALIDATOR SCRIPT (conditional) ───────────────
    let validate_script = if html.contains("vloValidate") || html.contains("v-validate") {
        r###"<script>
window.vloValidate = function(form, rules) {
    form.querySelectorAll('.vlo-error-msg').forEach(e => e.remove());
    let isValid = true;
    rules.forEach(ruleObj => {
        const input = form.elements[ruleObj.field];
        if (!input) return;
        const val = input.value.trim();
        const fieldRules = ruleObj.rules.split('|');
        for (const r of fieldRules) {
            if (r === 'required' && !val) { showErr(input, "This field is required"); isValid = false; break; }
            if (r === 'email' && val && !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(val)) { showErr(input, "Invalid email address"); isValid = false; break; }
            if (r.startsWith('min:')) { const min = parseFloat(r.split(':')[1]); if (val && parseFloat(val) < min) { showErr(input, "Must be at least " + min); isValid = false; break; } }
            if (r.startsWith('max:')) { const max = parseFloat(r.split(':')[1]); if (val && parseFloat(val) > max) { showErr(input, "Must be at most " + max); isValid = false; break; } }
        }
    });
    if (!isValid) return false;
    form.onsubmit = null;
    form.submit();
    return false;
};
function showErr(input, msg) {
    input.style.borderColor = "#ff4444";
    const err = document.createElement('div');
    err.className = 'vlo-error-msg';
    err.style.cssText = "color:#ff4444;font-size:0.75rem;margin-top:4px;";
    err.textContent = msg;
    input.parentNode.insertBefore(err, input.nextSibling);
}
</script>"###
    } else {
        ""
    };

    // ─── Single-pass injections ────────────────────────────
    // 🔥 Detect opt-in marker BEFORE injecting anything
    // Pages that need the CSRF fetch patch declare:
    //   <meta name="vlo-csrf-patch" content="1">
    let needs_csrf_patch = html.contains(r#"<meta name="vlo-csrf-patch""#);
    if needs_csrf_patch {
        // Strip the marker so it doesn't leak to the browser
        html = html.replace(r#"<meta name="vlo-csrf-patch" content="1">"#, "");
        html = html.replace(r#"<meta name="vlo-csrf-patch" content="1" />"#, "");
    }

    let mut head_injections = String::new();
    if !csrf_meta.is_empty() {
        head_injections.push_str(&csrf_meta);
        head_injections.push('\n');
        // 🔥 Only inject the patch when the page declared it needs it
        if needs_csrf_patch {
            head_injections.push_str(VLO_FETCH_CSRF_PATCH_JS);
            head_injections.push('\n');
        }
    }
    if !style_block.is_empty() {
        head_injections.push_str(&style_block);
        head_injections.push('\n');
    }

    let mut body_injections = String::new();
    if !flash_html.is_empty() {
        body_injections.push_str(&flash_html);
        body_injections.push('\n');
    }
    if !script_block.is_empty() {
        body_injections.push_str(&script_block);
        body_injections.push('\n');
    }
    if !active_link_js.is_empty() {
        body_injections.push_str(active_link_js);
        body_injections.push('\n');
    }
    if !hmr.is_empty() {
        body_injections.push_str(hmr);
        body_injections.push('\n');
    }
    if !validate_script.is_empty() {
        body_injections.push_str(validate_script);
        body_injections.push('\n');
    }

    // SSE init: dev-mode / build-time detection. In serve mode the real
    // injection happens in server::serve_build_page.
    if html.contains("class=\"vlo-sync\"") || html.contains("data-channel=") {
        let csrf_token = if dev {
            rendered
                .template_context
                .get("csrf_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        } else {
            "__VLO_CSRF_PLACEHOLDER__"
        };

        let mut sse_script = SSE_SCRIPT.to_string();
        sse_script = sse_script.replace("__CSRF_TOKEN__", csrf_token);
        body_injections.push_str(&sse_script);
        body_injections.push('\n');
    }

    // AJAX core (modular).
    if html.contains("__VLO_AJAX_Q") {
        let mut ajax_scripts = format!("<script>{}</script>", crate::modifier::AJAX_CORE_BASE);
        if html.contains("data-load-more") || html.contains("data-append") {
            ajax_scripts.push_str(&format!(
                "\n<script>{}</script>",
                crate::modifier::AJAX_CORE_LOADMORE
            ));
        }
        body_injections.push_str(&ajax_scripts);
        body_injections.push('\n');
    }

    // Apply (2 splices).
    if !head_injections.is_empty() {
        if let Some(i) = html.find("</head>") {
            html.insert_str(i, &head_injections);
        }
    }
    if !body_injections.is_empty() {
        if let Some(i) = html.rfind("</body>") {
            html.insert_str(i, &body_injections);
        }
    }

    html
}

// Duplicated from server.rs so dev mode has the same SSE script. Kept in
// sync manually — if you change one, change the other.
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
          els[i].innerHTML = data.value;
          els[i].classList.add('vlo-sync-updated');
          setTimeout((function(el){ return function(){ el.classList.remove('vlo-sync-updated'); }; })(els[i]), 600);
        }
      }
    } catch(err) { console.error('VLO SSE Error:', err); }
  };
  window.__VLO_SSE__.onerror = function(e) { console.error('VLO SSE connection error:', e); };
})();
</script>"#;

const VLO_FETCH_CSRF_PATCH_JS: &str = r#"<script>
(function(){
if(window.__VLO_FETCH_PATCHED__)return;window.__VLO_FETCH_PATCHED__=!0;
function sameOrigin(i){try{var u=typeof i=="string"?i:i instanceof URL?i.href:i instanceof Request?i.url:null;return u?new URL(u,location.href).origin===location.origin:!1}catch(_){return!1}}
function method(i,o){return String((o&&o.method)||(i&&i.method)||"GET").toUpperCase()}
function getH(h,k){
if(h instanceof Headers)return h.get(k);
if(Array.isArray(h)){var p=h.find(function(x){return String(x[0]).toLowerCase()===k.toLowerCase()});return p?p[1]:null}
if(h&&typeof h==="object"){var key=Object.keys(h).find(function(x){return x.toLowerCase()===k.toLowerCase()});return key?h[key]:null}
return null
}
function setH(h,k,v){
if(h instanceof Headers){h.set(k,v);return h}
if(Array.isArray(h)){var f=h.filter(function(x){return String(x[0]).toLowerCase()!==k.toLowerCase()});f.push([k,v]);return f}
h=h||{};
Object.keys(h).forEach(function(x){if(x.toLowerCase()===k.toLowerCase())delete h[x]});
h[k]=v;return h
}
var orig=window.fetch;
window.fetch=function(input,init){
init=init||{};
var m=method(input,init);
if(m!=="GET"&&m!=="HEAD"&&m!=="OPTIONS"&&sameOrigin(input)){
var t=document.querySelector('meta[name="csrf-token"]')?.content||"";
if(t){
var cur=getH(init.headers||{},"X-CSRF-Token");
if(cur==null||cur===""){init.headers=setH(init.headers||{},"X-CSRF-Token",t)}
}
}
return orig.call(this,input,init);
};
})();
</script>"#;

pub async fn ajax_js_handler() -> impl IntoResponse {
    let combined = format!(
        "{}\n{}",
        crate::modifier::AJAX_CORE_BASE,
        crate::modifier::AJAX_CORE_LOADMORE
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        combined,
    )
}

// ─── Global broadcast channel ──────────────────────────────
pub static LIVE_BROADCAST: OnceLock<broadcast::Sender<String>> = OnceLock::new();

pub fn init_live_broadcast() -> broadcast::Sender<String> {
    let (tx, _) = broadcast::channel(100);
    LIVE_BROADCAST.set(tx.clone()).ok();
    tx
}

pub fn broadcast_live(channel: &str, payload: &Value) {
    if let Some(tx) = LIVE_BROADCAST.get() {
        let _ = tx.send(
            serde_json::json!({
                "channel": channel,
                "value": payload
            })
            .to_string(),
        );
    }
}

pub async fn sse_handler() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        if let Some(mut rx) = LIVE_BROADCAST.get().map(|tx| tx.subscribe()) {
            while let Ok(msg) = rx.recv().await {
                yield Ok(Event::default().data(msg));
            }
        } else {
            yield Ok(Event::default().comment("VLO SSE Not Initialized"));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ─────────────────────────────────────────────────────────────
// HMR & file watching
// ─────────────────────────────────────────────────────────────
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
        let Ok(event) = result else { continue };
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
                // Clear all render caches so next request re-reads from disk.
                if let Ok(mut cache) = state::TEMPLATE_CACHE.lock() {
                    cache.clear();
                }
                crate::component::clear_component_path_cache();
                clear_layout_cache();
                clear_layout_flash_cache();
                let _ = tx.send(());
                println!("⚡ Reload");
            }
        }
    }
    Ok(())
}