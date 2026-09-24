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
    regex::Regex::new(r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)data-source\s*=\s*["']([^"']+)["']([^>]*?)>"#).unwrap()
});

static DATA_RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
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
        let variants = [format!("{}Layout", base), format!("{}Layout", capitalize(base)), format!("{}Layout", base.to_ascii_lowercase())];
        for variant in &variants {
            let path = dir.join(format!("{}.vlo", variant));
            if path.exists() { return variant.clone(); }
        }
    }
    "BaseLayout".to_string()
}

fn apply_layout(page_path: &str, source: String) -> (String, String) {
    if page_path.is_empty() { return (source, "BaseLayout".to_string()); }
    let layout_name = resolve_layout_name(page_path);
    if layout_name == "BaseLayout" { return (source, layout_name); }

    let open_base = "<BaseLayout";
    let close_base = "</BaseLayout>";
    let open_target = format!("<{}", layout_name);
    let close_target = format!("</{}>", layout_name);

    if source.contains(open_base) {
        let swapped = source.replace(open_base, &open_target).replace(close_base, &close_target);
        (swapped, layout_name)
    } else if source.contains(&open_target) {
        (source, layout_name)
    } else {
        (format!("<{}>{}</{}>", layout_name, source, layout_name), layout_name)
    }
}

// ---------------------------------------------------------------------------
// Data-source resolution
// ---------------------------------------------------------------------------
pub fn resolve_data_sources(source: &str, page_context: &HashMap<String, Value>) -> (String, HashMap<String, Value>) {
    let re = &*DATA_SOURCE_RE;
    let mut result = String::with_capacity(source.len());
    let mut last_end = 0usize;
    let mut all_computed: HashMap<String, Value> = HashMap::new();

    for cap in re.captures_iter(source) {
        let full = cap.get(0).unwrap();
        if full.start() < last_end { continue; }

        let tag = cap.get(1).unwrap().as_str();
        let before = cap.get(2).unwrap().as_str();
        let action = cap.get(3).unwrap().as_str().trim_start_matches("/api/").trim_matches('/').to_string();
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
                    if source.get(after_open..).map(|v| v.starts_with('>') || v.starts_with(' ') || v.starts_with('/')).unwrap_or(false) { depth += 1; }
                    cursor = after_open;
                }
                (_, Some(c)) => {
                    depth -= 1;
                    if depth == 0 { close_start = Some(c); break; }
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
        let (rendered, computed) = evaluate_data_source_block(inner, &action, page_context);
        
        for (key, value) in computed { all_computed.insert(key, value); }
        result.push_str(&format!("<{}{}{}>{}</{}>", tag, before, after, rendered, tag));
        last_end = close_pos + close.len();
    }

    result.push_str(&source[last_end..]);
    (result, all_computed)
}

pub fn fetch_api_data_sync(action: &str, params: &HashMap<String, Value>) -> Value {
    let action = action.to_string();
    let params = params.clone();
    let fetch_logic = async move {
        let actions = load_api_actions().unwrap_or_default();
        if let Some(sql_template) = actions.get(&action) {
            if let Some(pool) = DB_POOL.get() {
                let mut sql_params = serde_json::Map::new();
                for (key, value) in &params { sql_params.insert(key.clone(), value.clone()); }

                // ─── ENSURE PAGINATION DEFAULTS ─────────────────────────
                if !sql_params.contains_key("limit") { sql_params.insert("limit".to_string(), Value::Number(20.into())); }
                if !sql_params.contains_key("page") { sql_params.insert("page".to_string(), Value::Number(1.into())); }
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
                // ─────────────────────────────────────────────────────────
                // ─── SEARCH INJECTION (dynamic columns) ─────────────────
                let mut sql = sql_template.clone();
                
                crate::vlo_debug!("🔍 SORT/SEARCH [{}]: Original SQL = {}", action, sql);
                
                if let Some(search_val) = params.get("search") {
                    if let Some(search) = search_val.as_str() {
                        if !search.trim().is_empty() {
                            let safe_search = search.replace("'", "''").replace('%', "\\%").replace('_', "\\_");
                            
                            // Read columns from _columns param, default to "title,description"
                            let columns_str = params.get("_columns")
                                .and_then(|v| v.as_str())
                                .unwrap_or("title,description");
                            
                            // Validate column names (only alphanumeric and underscore)
                            let column_list: Vec<&str> = columns_str
                                .split(',')
                                .map(|s| s.trim())
                                .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                                .collect();
                            
                            if !column_list.is_empty() {
                                let like_parts: Vec<String> = column_list
                                    .iter()
                                    .map(|col| format!("{} LIKE '%{}%'", col, safe_search))
                                    .collect();
                                
                                let where_clause = format!(" WHERE ({})", like_parts.join(" OR "));
                                
                                crate::vlo_debug!("🔍 SORT/SEARCH [{}]: Search columns = {:?}, WHERE = {}", action, column_list, where_clause);
                                
                                let upper = sql.to_uppercase();
                                if let Some(pos) = upper.find(" ORDER BY ") {
                                    sql = format!("{}{}{}", &sql[..pos], where_clause, &sql[pos..]);
                                } else if let Some(pos) = upper.find(" LIMIT ") {
                                    sql = format!("{}{}{}", &sql[..pos], where_clause, &sql[pos..]);
                                } else {
                                    sql = format!("{}{}", sql.trim_end_matches(';'), where_clause);
                                }
                            }
                        }
                    }
                }
                
                // ─── SORT INJECTION ───────────────────────────────────────
                if let Some(sort_val) = params.get("sort") {
                    if let Some(sort_col) = sort_val.as_str() {
                        if !sort_col.trim().is_empty() && sort_col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                            let order = params.get("order")
                                .and_then(|v| v.as_str())
                                .unwrap_or("asc");
                            let safe_order = if order.eq_ignore_ascii_case("desc") { "DESC" } else { "ASC" };
                            let order_clause = format!(" ORDER BY {} {}", sort_col, safe_order);
                            
                            crate::vlo_debug!("🔍 SORT/SEARCH [{}]: Sort = {} {}", action, sort_col, safe_order);
                            
                            let upper = sql.to_uppercase();
                            if let Some(pos) = upper.find(" ORDER BY ") {
                                let after_order = &sql[pos..];
                                let after_upper = after_order.to_uppercase();
                                let end_offset = after_upper.find(" LIMIT ")
                                    .or_else(|| after_upper.find(";"))
                                    .unwrap_or(after_order.len());
                                sql = format!("{}{}{}", &sql[..pos], order_clause, &after_order[end_offset..]);
                            } else if let Some(pos) = upper.find(" LIMIT ") {
                                sql = format!("{}{}{}", &sql[..pos], order_clause, &sql[pos..]);
                            } else {
                                sql = format!("{}{}", sql.trim_end_matches(';'), order_clause);
                            }
                        }
                    }
                }
                
                crate::vlo_debug!("🔍 SORT/SEARCH [{}]: Final SQL = {}", action, sql);
                // ─────────────────────────────────────────────────────────
                // ─────────────────────────────────────────────────────────

                if let Ok(mut res) = execute_api_sql(pool, &sql, &sql_params).await {
                    // ─── AUTO-PAGINATION FOR DATA-SOURCE BLOCKS ─────────────
                    if action.starts_with("get_") {
                        let count_sql = crate::api::generate_count_sql(&sql);
                        if !count_sql.is_empty() {
                            if let Ok(count_res) = execute_api_sql(pool, &count_sql, &sql_params).await {
                                if let Some(count_data) = count_res.get("data").and_then(|d| d.as_array()) {
                                    if let Some(first_row) = count_data.first() {
                                        if let Some(total) = first_row.get("total").and_then(|v| v.as_i64()) {
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
                                                crate::vlo_debug!("✅ PAGINATION [{}]: {} total records, page {} of {}", action, total, page, total_pages);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // ─────────────────────────────────────────────────────────
                    return res; 
                }
            }
        }
        serde_json::json!({ "data": [], "success": false })
    };

    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || DATA_RT.block_on(fetch_logic)).join().unwrap_or(serde_json::json!({ "data": [], "success": false }))
    } else {
        DATA_RT.block_on(fetch_logic)
    }
}

pub fn evaluate_data_source_block(inner: &str, action: &str, page_context: &HashMap<String, Value>) -> (String, HashMap<String, Value>) {
    let api_response = fetch_api_data_sync(action, page_context);
    let mut context = page_context.clone();
    let mut computed: HashMap<String, Value> = HashMap::new();

    let var_name = action
        .trim_start_matches("get_").trim_start_matches("post_").trim_start_matches("put_")
        .trim_start_matches("patch_").trim_start_matches("delete_");

    // 1. Extract the "data" array for the loop variable (e.g., "users", "products")
    let data = api_response.get("data").cloned().unwrap_or(Value::Array(vec![]));
    context.insert(var_name.to_string(), data);

    // 2. Extract the "pagination" object and inject its fields globally
    if let Some(pagination) = api_response.get("pagination").and_then(|p| p.as_object()) {
        for (key, value) in pagination {
            context.insert(key.clone(), value.clone());
            computed.insert(key.clone(), value.clone());
        }
    }

    let rendered = render_control_flow(inner, &context);
    (rendered, computed)
}

// ---------------------------------------------------------------------------
// Rendering entry points
// ---------------------------------------------------------------------------
pub fn render_vlo(source: String) -> RenderedPage { render_vlo_with_query(source, &HashMap::new()) }

#[allow(dead_code)]
pub fn render_vlo_for_page(page_path: &str, source: String) -> RenderedPage { render_vlo_with_query_at(page_path, source, &HashMap::new()) }

#[allow(dead_code)]
pub fn render_vlo_for_build(source: String) -> RenderedPage { render_vlo_for_build_at("", source) }

pub fn render_vlo_for_build_at(page_path: &str, source: String) -> RenderedPage {
    let mut context = RenderedPage::default();
    let url_path = if page_path.is_empty() || page_path == "home" || page_path == "index" { "/".to_string() } else { format!("/{}", page_path.trim_matches('/')) };
    context.insert("@path", Value::String(url_path));

    let mut source = strip_server_block(&source);
    let (laid_out, layout_name) = apply_layout(page_path, source);
    source = laid_out;

    let (source_with_protected_query, runtime_query_interpolations) = preserve_runtime_query_interpolations(&source);
    source = source_with_protected_query;

    let mut runtime_interpolations = Vec::new();
    let interpolation_re = regex::Regex::new(r"\{\{\s*([a-zA-Z0-9_@.-]+)\s*\}\}").unwrap();
    let mut protected_source = String::with_capacity(source.len());
    let mut last_end = 0usize;

    for capture in interpolation_re.captures_iter(&source) {
        let full = capture.get(0).unwrap();
        let expression = capture.get(1).unwrap().as_str();
        if expression.contains('.') {
            protected_source.push_str(&source[last_end..full.start()]);
            let index = runtime_interpolations.len();
            runtime_interpolations.push(full.as_str().to_string());
            protected_source.push_str(&format!("__VLO_RUNTIME_DOTTED_INTERPOLATION_{}__", index));
            last_end = full.end();
        }
    }
    protected_source.push_str(&source[last_end..]);
    source = protected_source;

    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) { context.add_style("page", style.as_str()); }
    }
    source = STYLE_RE.replace_all(&source, "").into_owned();

    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, &layout_name, &mut context);
        if layout_name != "BaseLayout" { source = render_tag(&source, "BaseLayout", &mut context); }
        source = render_components(&source, &mut context);
        if source == previous { break; }
    }

    source = crate::server::resolve_directives(&source);

    let (source_with_runtime_data_sources, runtime_data_sources) = preserve_runtime_data_sources(&source);
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

pub fn render_vlo_with_query(source: String, query: &HashMap<String, String>) -> RenderedPage {
    render_vlo_with_query_at("", source, query)
}

pub fn render_vlo_with_query_at(page_path: &str, source: String, query: &HashMap<String, String>) -> RenderedPage {
    let mut context = RenderedPage::default();
    for (key, value) in query {
        if key == "_flash_messages" { continue; }
        if let Ok(n) = value.parse::<i64>() { context.insert(key, Value::Number(n.into())); } 
        else { context.insert(key, Value::String(value.clone())); }
    }
    
    // Inject flash messages if present
    if let Some(flash_str) = query.get("_flash_messages") {
        if let Ok(flashes) = serde_json::from_str::<Vec<Value>>(flash_str) {
            context.insert("flash_messages", Value::Array(flashes.clone()));
            
            if let Some(first) = flashes.first() {
                if let Some(obj) = first.as_object() {
                    for (key, value) in obj {
                        context.insert(&format!("flash_{}", key), value.clone());
                    }
                    if let Some(v) = obj.get("variant") { context.insert("variant", v.clone()); }
                    if let Some(v) = obj.get("icon") { context.insert("icon", v.clone()); }
                    if let Some(v) = obj.get("title") { context.insert("title", v.clone()); }
                    if let Some(v) = obj.get("description") { context.insert("description", v.clone()); }
                    
                    let desc_len = obj.get("description")
                        .and_then(|v| v.as_str())
                        .map(|s| s.chars().count())
                        .unwrap_or(0);
                    let duration = if desc_len < 30 { "short" }
                    else if desc_len < 80 { "medium" }
                    else { "long" };
                    context.insert("duration", Value::String(duration.to_string()));
                }
            }
        }
    }
    
    let url_path = if page_path.is_empty() || page_path == "home" || page_path == "index" { "/".to_string() } else { format!("/{}", page_path.trim_matches('/')) };
    context.insert("@path", Value::String(url_path));

    let mut source = strip_server_block(&source);
    let (laid_out, layout_name) = apply_layout(page_path, source);
    source = laid_out;

    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) { context.add_style("page", style.as_str()); }
    }
    source = STYLE_RE.replace_all(&source, "").into_owned();

    // ─── STEP 1: Resolve data-sources (fetches API data, expands {for} loops with local context) ───
    let (resolved_source, computed_vars) = resolve_data_sources(&source, &context.template_context);
    source = resolved_source;
    
    for (key, value) in computed_vars {
        context.insert(&key, value);
    }

    // ─── STEP 2: Layout + Component rendering loop ───
    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, &layout_name, &mut context);
        if layout_name != "BaseLayout" { source = render_tag(&source, "BaseLayout", &mut context); }
        source = render_components(&source, &mut context);
        if source == previous { break; }
    }

    // ─── STEP 3: Control flow ({if}, {for}) — expands flash loops, generates <Toast> tags ───
    source = render_control_flow(&source, &context.template_context);

    // ─── STEP 4: Second component pass — renders <Toast> tags generated by {for} loops ───
    source = render_components(&source, &mut context);

    // ─── STEP 5: Directives (v-post, v-put, v-delete) ───
    source = crate::server::resolve_directives(&source);

    if query.contains_key("status") || query.contains_key("action") {
        source.push_str(r#"<script>
(function () { if (window.history && window.history.replaceState) { window.history.replaceState({}, document.title, window.location.pathname); } })();
</script>"#);
    }

    context.html = clean_empty_tags(&source);
    context
}
// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------
pub async fn home_handler(
    axum::Extension(auth): axum::Extension<crate::auth::AuthUser>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let flash_encoded = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| crate::auth::parse_cookie_header(h, crate::auth::FLASH_COOKIE));
    render_page("home".to_string(), query, auth, flash_encoded).await
}

pub async fn page_handler(
    AxumPath(path): AxumPath<String>,
    axum::Extension(auth): axum::Extension<crate::auth::AuthUser>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let flash_encoded = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| crate::auth::parse_cookie_header(h, crate::auth::FLASH_COOKIE));
    render_page(path, query, auth, flash_encoded).await
}

pub async fn not_found_handler() -> impl IntoResponse { render_404().await }

fn layout_uses_flash(page_path: &str) -> bool {
    let root = state::get_project_root();
    let layouts_dir = root.join("layouts");
    let page = Path::new(page_path);
    let stem = page.file_stem().and_then(|s| s.to_str()).unwrap_or(page_path);
    let parent = page.parent().filter(|p| !p.as_os_str().is_empty());

    let mut candidates: Vec<PathBuf> = Vec::new();
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
    auth: crate::auth::AuthUser, // MUST be `auth`, not `_auth`
    flash_encoded: Option<String>,
) -> impl IntoResponse {
    let page_path = path.clone();
    match tokio::task::spawn_blocking(move || -> Option<Response> {
        let file = state::get_project_root().join("pages").join(format!("{}.vlo", page_path));
        let content = match fs::read_to_string(&file) { Ok(c) => c, Err(_) => return None };
        let mut query = query;

        // ─── PAGE GUARD CHECK (RESTORED) ────────────────────
        if let Some(guard) = crate::auth::extract_page_guard(&content) {
            let current_url = if page_path == "home" { "/".to_string() } else { format!("/{}", page_path) };
            let next = query.get("next").cloned();
            if let Some(response) = crate::auth::check_page_guard(&guard, &auth.user, &current_url, next.as_deref()) {
                return Some(response);
            }
        }
        // ────────────────────────────────────────────────────

        // ─── AUTH & CSRF INJECTION (RESTORED) ───────────────
        let cfg = crate::auth::auth_config();
        query.insert("auth_identifier_field".to_string(), cfg.identifier_field.clone());
        query.insert("auth_password_field".to_string(), cfg.password_field.clone());

        match &auth.user {
            Some(user) => {
                query.insert("logged_in".to_string(), "true".to_string());
                query.insert("user_name".to_string(), user.name.clone());
                query.insert("user_role".to_string(), user.role.clone());
                query.insert("user_email".to_string(), user.email.clone());
            }
            None => { query.insert("logged_in".to_string(), String::new()); }
        }

        if let Some(csrf) = &auth.csrf_token { query.insert("csrf_token".to_string(), csrf.clone()); } 
        else { query.insert("csrf_token".to_string(), String::new()); }
        // ────────────────────────────────────────────────────

        // ─── PAGINATION VARIABLES (RESTORED) ────────────────
        let page: i64 = query.get("page").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
        let limit: i64 = query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20).max(1).min(100);
        let offset = (page - 1) * limit;
        query.insert("page".to_string(), page.to_string());
        query.insert("limit".to_string(), limit.to_string());
        query.insert("offset".to_string(), offset.to_string());
        query.insert("prev_page".to_string(), (page - 1).max(1).to_string());
        query.insert("next_page".to_string(), (page + 1).to_string());
        // ────────────────────────────────────────────────────
        // ─── SEARCH & SORT DEFAULTS ─────────────────────────
        if !query.contains_key("order") {
            query.insert("order".to_string(), "asc".to_string());
        }
        // ────────────────────────────────────────────────────

        // ─── FLASH DETECTION ────────────────────────────────
        let page_handles_flash = content.contains("flash_messages")
            || layout_uses_flash(&page_path);

        let flash_consumed = if let Some(encoded) = &flash_encoded {
            if let Some(flash) = crate::auth::decode_flash(encoded) {
                query.insert("_flash_messages".to_string(), serde_json::to_string(&vec![flash]).unwrap_or_default());
                true
            } else { false }
        } else { false };
        // ────────────────────────────────────────────────────

        let rendered = render_vlo_with_query_at(&page_path, content, &query);

        let auto_flash = !page_handles_flash;
        let mut response = (StatusCode::OK, Html(wrap_html(&page_path, &rendered, auto_flash))).into_response();

        if flash_consumed {
            let expire_cookie = crate::auth::expire_flash_cookie();
            if let Ok(val) = axum::http::HeaderValue::from_str(&expire_cookie) {
                response.headers_mut().append(axum::http::header::SET_COOKIE, val);
            }
        }

        Some(response)
    }).await {
        Ok(Some(response)) => response,
        _ => render_404().await.into_response(),
    }
}

pub async fn render_404() -> impl IntoResponse {
    tokio::task::spawn_blocking(move || {
        let file = state::get_project_root().join("pages").join("404.vlo");
        if let Ok(content) = fs::read_to_string(file) {
            let rendered = render_vlo(content);
            (StatusCode::NOT_FOUND, Html(wrap_html("404 - Page Not Found", &rendered, true))).into_response()
        } else {
            let rendered = RenderedPage { html: r#"<div class="not-found"><h1>404</h1><p>Page Not Found</p><a href="/">Back to Home</a></div>"#.to_string(), ..Default::default() };
            (StatusCode::NOT_FOUND, Html(wrap_html("404 - Page Not Found", &rendered, true))).into_response()
        }
    }).await.unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, Html("Server Error".to_string())).into_response())
}

pub fn wrap_html(title: &str, rendered: &RenderedPage, auto_flash: bool) -> String {
    let dev = state::app_mode().is_dev();
    let component_styles = if rendered.styles.is_empty() { String::new() } else { format!("\n<style>\n{}\n</style>", rendered.styles.join("\n")) };
    
    let hmr = if dev {
        r#"<script>
        (requestIdleCallback || setTimeout)(function() {
        const es = new EventSource("/__vlo_hmr");
        es.onmessage = function() { location.reload(); };
        window.addEventListener("beforeunload", function() { es.close(); });
        }, 100);
        </script>"#
    } else { "" };

    let csrf_meta = if let Some(csrf) = rendered.template_context.get("csrf_token") {
        if let Some(token) = csrf.as_str() {
            if !token.is_empty() { format!(r#"<meta name="csrf-token" content="{}">"#, token) } else { String::new() }
        } else { String::new() }
    } else { String::new() };

    // ─── FLASH: Only auto-render if page doesn't handle it ───
    let flash_html = if auto_flash {
        if let Some(flashes) = rendered.template_context.get("flash_messages") {
            if let Some(arr) = flashes.as_array() {
                let mut html = String::new();
                for flash in arr {
                    let variant = flash.get("variant").and_then(|v| v.as_str()).unwrap_or("success");
                    let icon = flash.get("icon").and_then(|v| v.as_str()).unwrap_or("✅");
                    let title = flash.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let description = flash.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let border_color = match variant {
                        "success" => "#00ff88",
                        "error" => "#ff4444",
                        "warning" => "#ffaa00",
                        _ => "#00f5ff",
                    };
                    html.push_str(&format!(
                        r#"<div class="vlo-flash" style="position:fixed;top:20px;right:20px;z-index:99999;padding:16px 22px;border-radius:10px;background:#1a1a2e;border-left:4px solid {};color:#fff;box-shadow:0 8px 24px rgba(0,0,0,0.4);display:flex;align-items:center;gap:12px;max-width:380px;animation:vloFlashIn 0.35s ease">
<span style="font-size:1.4rem">{}</span>
<div><strong style="display:block;font-size:0.9rem;margin-bottom:2px">{}</strong><span style="color:#aaa;font-size:0.78rem">{}</span></div>
</div>"#,
                        border_color, icon, title, description
                    ));
                }
                if !html.is_empty() {
                    html.push_str(r#"<style>@keyframes vloFlashIn{from{opacity:0;transform:translateX(40px)}to{opacity:1;transform:translateX(0)}}</style>
<script>setTimeout(function(){document.querySelectorAll('.vlo-flash').forEach(function(el){el.style.transition='opacity 0.4s';el.style.opacity='0';setTimeout(function(){el.remove()},400)})},4000)</script>"#);
                }
                html
            } else { String::new() }
        } else { String::new() }
    } else {
        String::new() // Page handles flash rendering itself
    };
    // ─────────────────────────────────────────────────────────

    let mut html = rendered.html.clone();
    html = html.replace("{{title}}", title);
    
    if !csrf_meta.is_empty() { html = html.replacen("</head>", &format!("{}\n</head>", csrf_meta), 1); }
    if !component_styles.is_empty() { html = html.replacen("</head>", &format!("{}\n</head>", component_styles), 1); }
    if !flash_html.is_empty() { html = html.replacen("</body>", &format!("{}\n</body>", flash_html), 1); }
    if !hmr.is_empty() { html = html.replacen("</body>", &format!("{}\n</body>", hmr), 1); }
    html
}

// ---------------------------------------------------------------------------
// HMR & file watching
// ---------------------------------------------------------------------------
pub async fn hmr_handler(tx: broadcast::Sender<()>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = tx.subscribe();
    let stream = async_stream::stream! { while rx.recv().await.is_ok() { yield Ok(Event::default().data("reload")); } };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub fn watch_files(pages: PathBuf, public: PathBuf, tx: broadcast::Sender<()>, last: Arc<Mutex<Instant>>) -> notify::Result<()> {
    let root = state::get_project_root();
    let layouts = root.join("layouts");
    let components = root.join("components");
    let (tx_notify, rx) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx_notify, Config::default())?;
    let paths_to_watch = [&pages, &public, &layouts, &components];
    for path in paths_to_watch { if path.exists() { watcher.watch(path, RecursiveMode::Recursive)?; } }
    
    for result in rx {
        let Ok(event) = result else { continue; };
        let relevant = event.paths.iter().any(|path| {
            path.extension().and_then(|e| e.to_str()) == Some("vlo") || path.starts_with(&public) || path.starts_with(&layouts) || path.starts_with(&components)
        });
        if !relevant { continue; }
        if let Ok(mut timestamp) = last.try_lock() {
            if timestamp.elapsed().as_millis() > 200 {
                *timestamp = Instant::now();
                if let Ok(mut cache) = state::TEMPLATE_CACHE.lock() { cache.clear(); }
                let _ = tx.send(());
                println!("⚡ Reload");
            }
        }
    }
    Ok(())
}