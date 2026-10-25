// ─────────────────────────────────────────────────────────────
// component.rs — VLO component rendering engine
//
// Performance notes (behavior preserved):
//  * props are overlaid on the page context and restored afterwards, instead
//    of deep-cloning the whole context twice per component instance
//  * per-template facts (used props, <button> default) computed once and cached
//  * byte-jumping tag scanners with borrowed slices (no format!/String per scan)
//  * nested-render loop tracks "did anything change" instead of copying and
//    comparing the whole page every pass
//  * debug output goes through vlo_debug! (the API response is no longer
//    formatted and printed on every render)
//  * module detection checks <project>/modules, not the substring "modules"
// ─────────────────────────────────────────────────────────────
use crate::{
    api::strip_server_block,
    state::{
        CompiledTemplate, RenderedPage, CLASS_RE, ELEMENT_RE, SLOT_RE,
        STYLE_RE, TEMPLATE_CACHE,
    },
    template::{
        clean_empty_tags, escape_html_attribute, mark_safe_html,
        render_control_flow_in_scope, render_interpolations,
    },
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, RwLock},
};

// ─── PATH CACHE: avoids repeated filesystem probes per tag ───────────
// Value: resolved path + "is this a module component" flag (computed once).
type PathEntry = Option<(PathBuf, bool)>;

static PATH_CACHE: LazyLock<RwLock<HashMap<String, PathEntry>>> =
    LazyLock::new(Default::default);

fn component_info(name: &str) -> PathEntry {
    if let Some(hit) = PATH_CACHE.read().unwrap().get(name) {
        return hit.clone();
    }
    let found = resolve_component_path_internal(name).map(|p| {
        let is_module = is_module_path(&p);
        (p, is_module)
    });
    PATH_CACHE
        .write()
        .unwrap()
        .insert(name.to_string(), found.clone());
    found
}

/// Cached public entry point. Resolves once per component name, then reuses.

/// Clears the caches. Called by the file watcher on HMR reloads.
pub fn clear_component_path_cache() {
    PATH_CACHE.write().unwrap().clear();
    META_CACHE.write().unwrap().clear();
}

/// Alias kept for callers that use the shorter name.
#[allow(dead_code)]
pub fn clear_path_cache() {
    clear_component_path_cache();
}

/// A component is a module component when it lives under `<project>/modules`.
/// (The old check was `path.contains("modules")`, which also matched any
/// project located in e.g. `my-modules/` or `node_modules/`.)
fn is_module_path(path: &Path) -> bool {
    let root = crate::state::get_project_root();
    match path.strip_prefix(&root) {
        Ok(rel) => rel.starts_with("modules"),
        Err(_) => path.to_string_lossy().contains("modules"),
    }
}

/// Internal resolver. Performs the actual filesystem lookups in the
/// same priority order as before.
fn resolve_component_path_internal(name: &str) -> Option<PathBuf> {
    let root = crate::state::get_project_root();
    let layouts = root.join("layouts");

    // 1. layouts/{name}.vlo
    let direct = layouts.join(format!("{}.vlo", name));
    if direct.exists() {
        return Some(direct);
    }

    // 2. layouts/*/{name}.vlo
    if let Ok(entries) = fs::read_dir(&layouts) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let candidate = path.join(format!("{}.vlo", name));
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }

    // 3. components/{name}.vlo
    let component = root.join("components").join(format!("{}.vlo", name));
    if component.exists() {
        return Some(component);
    }

    // 4. modules/*/components/{name}.vlo
    if let Some(module_component) = crate::modules::find_module_component(name) {
        return Some(module_component);
    }

    None
}

pub fn read_component_template(path: &Path) -> Option<Arc<CompiledTemplate>> {
    // Fast path: cached.
    if let Ok(cache) = TEMPLATE_CACHE.lock() {
        if let Some(cached) = cache.get(path) {
            return Some(Arc::clone(cached));
        }
    }

    let source = fs::read_to_string(path).ok()?;
    let mut css = String::new();

    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) {
            css.push_str(style.as_str());
            css.push('\n');
        }
    }

    let template = STYLE_RE.replace_all(&source, "").into_owned();
    let compiled = Arc::new(CompiledTemplate { template, css });

    if let Ok(mut cache) = TEMPLATE_CACHE.lock() {
        cache.insert(path.to_path_buf(), Arc::clone(&compiled));
    }

    Some(compiled)
}

// ─── PER-TEMPLATE FACTS (computed once per compiled template) ─────────
struct TemplateMeta {
    /// Props referenced as a plain `{{ name }}` in the template.
    used: HashSet<String>,
    /// Template contains `<button` (case-insensitive) → default type="button".
    has_button: bool,
}

impl TemplateMeta {
    fn from_template(template: &str) -> Self {
        let mut used = HashSet::new();
        for captures in crate::state::PROP_RE.captures_iter(template) {
            used.insert(captures[1].to_string());
        }
        Self {
            used,
            has_button: contains_ci(template, "<button"),
        }
    }
}

/// Keyed by the compiled template's address. Each entry keeps its `Arc` alive,
/// so an address can never be reused while its entry exists; a reloaded
/// template gets a new Arc and therefore a fresh entry.
static META_CACHE: LazyLock<
    RwLock<HashMap<usize, (Arc<CompiledTemplate>, Arc<TemplateMeta>)>>,
> = LazyLock::new(Default::default);

fn template_meta(compiled: &Arc<CompiledTemplate>) -> Arc<TemplateMeta> {
    let key = Arc::as_ptr(compiled) as usize;
    if let Some((_, meta)) = META_CACHE.read().unwrap().get(&key) {
        return Arc::clone(meta);
    }
    let meta = Arc::new(TemplateMeta::from_template(&compiled.template));
    let mut cache = META_CACHE.write().unwrap();
    if cache.len() > 1024 {
        cache.clear();
    }
    cache.insert(key, (Arc::clone(compiled), Arc::clone(&meta)));
    meta
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    !n.is_empty()
        && h.len() >= n.len()
        && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

// ─── TAG RENDERING ───────────────────────────────────────────────────
fn render_tag_opt(source: &str, tag: &str, context: &mut RenderedPage) -> Option<String> {
    let (start, end, props, children) = find_tag_ref(source, tag)?;
    let rendered = render_component_file(tag, props, children, context);
    let mut out = String::with_capacity(source.len() - (end - start) + rendered.len());
    out.push_str(&source[..start]);
    out.push_str(&rendered);
    out.push_str(&source[end..]);
    Some(out)
}

pub fn render_tag(source: &str, tag: &str, context: &mut RenderedPage) -> String {
    render_tag_opt(source, tag, context).unwrap_or_else(|| source.to_string())
}

/// Returns `None` when nothing was rendered (no copy of the source is made).
fn render_components_opt(source: &str, context: &mut RenderedPage) -> Option<String> {
    // Fast path: nothing to render if there are no tags at all.
    if !source.contains('<') {
        return None;
    }

    let b = source.as_bytes();
    let mut output = String::new();
    let mut replaced = false;
    let mut last = 0usize;
    let mut i = 0usize;

    while i < b.len() {
        // Fast-forward to the next `<` in one call.
        let Some(rel) = source[i..].find('<') else { break };
        let pos = i + rel;
        i = pos + 1;

        if i >= b.len() || !b[i].is_ascii_uppercase() {
            continue;
        }

        let mut end = i;
        while end < b.len()
            && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] == b'-')
        {
            end += 1;
        }

        let tag = &source[i..end];

        // Validate tag boundary.
        if end < b.len() {
            let next_ch = b[end];
            if !(next_ch == b'>' || next_ch == b'/' || next_ch.is_ascii_whitespace()) {
                continue;
            }
        }

        if let Some((_, tag_end, props, children)) = find_tag_ref(&source[pos..], tag) {
            if !replaced {
                output.reserve(source.len());
                replaced = true;
            }
            output.push_str(&source[last..pos]);
            output.push_str(&render_component_file(tag, props, children, context));
            last = pos + tag_end;
            i = last;
        }
    }

    if !replaced {
        return None;
    }
    output.push_str(&source[last..]);
    Some(output)
}

/// Fast-scanning top-level component renderer.
pub fn render_components(source: &str, context: &mut RenderedPage) -> String {
    render_components_opt(source, context).unwrap_or_else(|| source.to_string())
}

// ─── SCANNERS (byte-jumping, borrowed slices) ────────────────────────
#[inline]
fn at_open(b: &[u8], i: usize, name: &[u8]) -> bool {
    b.get(i + 1..).map_or(false, |r| r.starts_with(name))
}

#[inline]
fn at_close(b: &[u8], i: usize, name: &[u8]) -> bool {
    b.get(i + 1) == Some(&b'/')
        && b.get(i + 2..).map_or(false, |r| r.starts_with(name))
        && b.get(i + 2 + name.len()) == Some(&b'>')
}

/// Borrowing `find_tag`: (start, end, props, children).
fn find_tag_ref<'a>(source: &'a str, name: &str) -> Option<(usize, usize, &'a str, &'a str)> {
    let b = source.as_bytes();
    let nb = name.as_bytes();

    // First occurrence of "<name" (any boundary), like `source.find("<name")`.
    let mut from = 0usize;
    let start = loop {
        let p = from + b.get(from..)?.iter().position(|&c| c == b'<')?;
        if at_open(b, p, nb) {
            break p;
        }
        from = p + 1;
    };
    let mut index = start + 1 + nb.len();

    if index < source.len() {
        let next = source[index..].chars().next()?;
        if !(next.is_whitespace() || next == '/' || next == '>') {
            return None;
        }
    }

    let props_start = index;
    let open_end = find_tag_opening_end(source, index)?;
    let props = &source[props_start..open_end];

    let self_closing = props.trim_end().ends_with('/') || is_void_tag(name);
    index = open_end + 1;

    if self_closing {
        return Some((start, index, props, ""));
    }

    let children_start = index;
    let mut depth: usize = 1;

    while index < b.len() {
        let Some(p) = b[index..].iter().position(|&c| c == b'<') else {
            index = b.len();
            break;
        };
        index += p;

        if at_close(b, index, nb) {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some((
                    start,
                    index + nb.len() + 3,
                    props,
                    &source[children_start..index],
                ));
            }
            index += nb.len() + 3;
            continue;
        }

        if at_open(b, index, nb) {
            let after_name = index + 1 + nb.len();
            let valid = source[after_name..]
                .chars()
                .next()
                .map(|ch| ch.is_whitespace() || ch == '>' || ch == '/')
                .unwrap_or(false);

            if valid {
                let opening_end = find_tag_opening_end(source, after_name)?;
                let nested_props = source[after_name..opening_end].trim();
                if !nested_props.ends_with('/') && !is_void_tag(name) {
                    depth += 1;
                }
                index = opening_end + 1;
                continue;
            }
        }

        index += 1;
    }

    Some((start, index, props, &source[children_start..]))
}

pub fn find_tag_opening_end(source: &str, start: usize) -> Option<usize> {
    let b = source.as_bytes();
    let mut quote: u8 = 0;
    let mut cursor = start;

    while cursor < b.len() {
        let c = b[cursor];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'>' {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

pub fn find_matching_tag_end(
    source: &str,
    start: usize,
    tag_name: &str,
) -> Option<usize> {
    let b = source.as_bytes();
    let nb = tag_name.as_bytes();
    let mut depth: usize = 1;
    let mut cursor = start;

    while cursor < b.len() {
        let p = b[cursor..].iter().position(|&c| c == b'<')?;
        cursor += p;

        if at_close(b, cursor, nb) {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(cursor + nb.len() + 3);
            }
            cursor += nb.len() + 3;
            continue;
        }

        if at_open(b, cursor, nb) {
            let after_name = cursor + 1 + nb.len();
            let valid = source[after_name..]
                .chars()
                .next()
                .map(|ch| ch.is_whitespace() || ch == '>' || ch == '/')
                .unwrap_or(false);

            if valid {
                let opening_end = find_tag_opening_end(source, after_name)?;
                let props = source[after_name..opening_end].trim();
                if !props.ends_with('/') && !is_void_tag(tag_name) {
                    depth += 1;
                }
                cursor = opening_end + 1;
                continue;
            }
        }

        cursor += 1;
    }
    None
}

// ─── COMPONENT FILE RENDERING ────────────────────────────────────────
pub fn render_component_file(
    name: &str,
    props_str: &str,
    children: &str,
    context: &mut RenderedPage,
) -> String {
    let (path, is_module_component) = match component_info(name) {
        Some(info) => info,
        None => return format!("<!-- Missing component: {} -->", name),
    };

    let compiled = match read_component_template(&path) {
        Some(template) => template,
        None => return format!("<!-- Missing template: {} -->", name),
    };

    // Fast path: empty component file.
    if compiled.template.is_empty() && compiled.css.trim().is_empty() {
        return String::new();
    }

    let props = parse_props_v7(props_str);

    // 1. Track module usage and CSS even during build so styles are injected.
    if is_module_component {
        if let Some(module_path) = path.parent().and_then(|p| p.parent()) {
            if let Some(module_name) = module_path.file_name().and_then(|n| n.to_str()) {
                context.add_used_module(module_name);
            }
        }
    }

    if !compiled.css.trim().is_empty() {
        context.add_style(name, compiled.css.trim());
    }

    // Preserve data-driven module components during build so they remain
    // in the HTML for `serve` to evaluate with live DB data.
    if is_module_component && crate::state::is_building() && props.contains_key("source") {
        let mut out = format!("<{}", name);
        let trimmed_props = props_str.trim();
        if !trimmed_props.is_empty() {
            out.push(' ');
            out.push_str(trimmed_props);
        }

        if children.is_empty() {
            if !trimmed_props.ends_with('/') && !is_void_tag(name) {
                out.push_str(" /");
            }
            out.push('>');
        } else {
            let clean_props = trimmed_props.trim_end_matches('/').trim();
            out = format!("<{}", name);
            if !clean_props.is_empty() {
                out.push(' ');
                out.push_str(clean_props);
            }
            out.push('>');
            out.push_str(children);
            out.push_str(&format!("</{}>", name));
        }
        return out;
    }

    let template = &compiled.template;
    let (raw_named_slots, raw_default_slot) = parse_slot_content(children);

    let mut named_slots = HashMap::with_capacity(raw_named_slots.len());
    for (slot_name, slot_content) in raw_named_slots {
        let rendered = render_nested_vlo_content(&slot_content, context);
        named_slots.insert(slot_name, rendered);
    }

    let default_slot = render_nested_vlo_content(&raw_default_slot, context);
    let default_slot = apply_active_nav_class(&default_slot, context);

    // ─── MODULE COMPONENT HANDLING (RUNTIME) ─────────────────────────
    if is_module_component {
        crate::vlo_debug!("🔥 [VLO] Module component: {}", name);
        if let Some(module_path) = path.parent().and_then(|p| p.parent()) {
            if let Some(module_name) = module_path.file_name().and_then(|n| n.to_str()) {
                context.add_used_module(module_name);
            }

            let raw_data = if let Some(source) = props.get("source").and_then(|v| v.as_str()) {
                let action = source.trim_start_matches("/api/").trim_matches('/');
                crate::vlo_debug!("📡 [VLO] Fetching API for {}: action='{}'", name, action);

                let api_response =
                    crate::router::fetch_api_data_sync(action, &context.template_context);
                crate::vlo_debug!("📡 [VLO] API Response for {}: {:?}", name, api_response);

                api_response
                    .get("data")
                    .cloned()
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default()
            } else {
                crate::vlo_debug!(
                    "⚠️ [VLO] No 'source' prop found for {}. Props: {:?}",
                    name,
                    props.keys().collect::<Vec<_>>()
                );
                Vec::new()
            };

            crate::vlo_debug!(
                "🔄 [VLO] Executing module handler for {} with {} rows",
                name,
                raw_data.len()
            );
            match crate::module_handler::execute_module_handler(module_path, &props, raw_data) {
                Ok(response) => {
                    crate::vlo_debug!(
                        "✅ [VLO] Handler produced {} transformed rows for {}",
                        response.data.len(),
                        name
                    );

                    context.insert("data", serde_json::json!(response.data));
                    context.insert("meta", serde_json::json!(response.meta));
                    context.insert("config", serde_json::json!(response.config));
                }
                Err(e) => {
                    println!("❌ [VLO] Module handler error for {}: {}", name, e);
                }
            }
        }
    }
    // ────────────────────────────────────────────────────────────────

    // Props + children are overlaid on the page context for the render and
    // removed again afterwards (no deep clone of the context).
    let rendered = render_component_template_scoped(
        template,
        &mut context.template_context,
        &props,
        default_slot.clone(),
    );
    let rendered = render_slots(&rendered, &named_slots, &default_slot);

    let incoming_class = props.get("class").and_then(|value| {
        if let Value::String(value) = value {
            Some(value.clone())
        } else {
            None
        }
    });

    let meta = template_meta(&compiled);
    let attributes = attributes_from_meta(&meta, &props);
    if attributes.is_empty() && incoming_class.is_none() {
        return rendered;
    }

    let trimmed = rendered.trim_start();
    let starts_with_style_or_script = trimmed.starts_with("<style")
        || trimmed.starts_with("<script");
    if starts_with_style_or_script {
        return rendered;
    }

    if let Some(captures) = ELEMENT_RE.captures(&rendered) {
        let full_match = captures.get(0).unwrap();
        let tag_name = captures.get(1).unwrap().as_str();
        let existing_attributes = captures
            .get(2)
            .map(|value| value.as_str())
            .unwrap_or("")
            .to_string();

        let (existing_attributes, class_attr) =
            if let Some(extra) = incoming_class
                .as_deref()
                .map(|class| class.trim())
                .filter(|class| !class.is_empty())
            {
                let extra = escape_html_attribute(extra);
                if let Some(class_match) = CLASS_RE.captures(&existing_attributes) {
                    let existing_value = class_match
                        .get(2)
                        .or_else(|| class_match.get(3))
                        .map(|value| value.as_str())
                        .unwrap_or("")
                        .trim();
                    let merged = if existing_value.is_empty() {
                        format!("class=\"{}\"", extra)
                    } else {
                        format!("class=\"{} {}\"", existing_value, extra)
                    };
                    let stripped = CLASS_RE
                        .replace(&existing_attributes, "")
                        .trim()
                        .to_string();
                    (stripped, Some(merged))
                } else {
                    (existing_attributes, Some(format!("class=\"{}\"", extra)))
                }
            } else {
                (existing_attributes, None)
            };

        let attributes = match class_attr {
            Some(class) if attributes.is_empty() => class,
            Some(class) => format!("{} {}", class, attributes),
            None => attributes,
        };

        if attributes.is_empty() {
            return rendered;
        }

        let replacement = if existing_attributes.trim().is_empty() {
            format!("<{} {}>", tag_name, attributes)
        } else {
            format!("<{} {} {}>", tag_name, existing_attributes.trim(), attributes)
        };

        return format!(
            "{}{}{}",
            &rendered[..full_match.start()],
            replacement,
            &rendered[full_match.end()..]
        );
    }

    rendered
}

pub fn render_slots(
    template: &str,
    named_slots: &HashMap<String, String>,
    default_slot: &str,
) -> String {
    // Fast path: no slot markers in the template.
    if !template.contains("<slot") {
        return template.to_string();
    }

    let result = SLOT_RE
        .replace_all(template, |captures: &regex::Captures| {
            let name = captures
                .get(1)
                .map(|value| value.as_str().trim())
                .unwrap_or("");

            let fallback = captures
                .get(2)
                .map(|value| value.as_str())
                .unwrap_or("");

            if name.is_empty() {
                if default_slot.trim().is_empty() {
                    fallback.to_string()
                } else {
                    default_slot.to_string()
                }
            } else if let Some(content) = named_slots.get(name) {
                content.clone()
            } else {
                fallback.to_string()
            }
        })
        .into_owned();

    if result.contains("<slot") {
        SLOT_RE.replace_all(&result, "").into_owned()
    } else {
        result
    }
}

pub fn parse_slot_content(children: &str) -> (HashMap<String, String>, String) {
    let mut named_slots = HashMap::new();
    let mut default_content = String::new();
    let mut cursor = 0usize;
    let mut default_start = 0usize;

    while cursor < children.len() {
        let open_start = match children[cursor..].find('<') {
            Some(offset) => cursor + offset,
            None => break,
        };

        let (_tag_name, _opening_end, element_end, props, content) =
            match parse_element_at_ref(children, open_start) {
                Some(info) => info,
                None => {
                    cursor = open_start + 1;
                    continue;
                }
            };

        // The props parser only runs when "slot" can possibly be a prop.
        if props.contains("slot") {
            if let Some(slot_name) = get_slot_name(props) {
                if open_start > default_start {
                    default_content.push_str(&children[default_start..open_start]);
                }
                named_slots
                    .entry(slot_name)
                    .or_insert_with(String::new)
                    .push_str(content);
                cursor = element_end;
                default_start = cursor;
                continue;
            }
        }

        cursor = element_end;
    }

    if default_start < children.len() {
        default_content.push_str(&children[default_start..]);
    }

    (named_slots, default_content)
}

pub fn get_slot_name(props: &str) -> Option<String> {
    parse_props_v7(props).get("slot").and_then(|value| {
        if let Value::String(value) = value {
            Some(value.clone())
        } else {
            None
        }
    })
}

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
    "source", "track", "wbr",
];

pub fn is_void_tag(name: &str) -> bool {
    VOID_TAGS.iter().any(|tag| name.eq_ignore_ascii_case(tag))
}

/// Borrowing `parse_element_at`: (tag, opening_end, element_end, props, content).
fn parse_element_at_ref(
    source: &str,
    start: usize,
) -> Option<(&str, usize, usize, &str, &str)> {
    let b = source.as_bytes();
    if b.get(start) != Some(&b'<') {
        return None;
    }

    let mut cursor = start + 1;
    if cursor >= b.len() {
        return None;
    }

    if matches!(b[cursor], b'/' | b'!' | b'?') {
        return None;
    }

    let tag_start = cursor;
    while cursor < b.len()
        && (b[cursor].is_ascii_alphanumeric() || matches!(b[cursor], b'-' | b'_' | b':'))
    {
        cursor += 1;
    }
    if cursor == tag_start {
        return None;
    }

    let tag_name = &source[tag_start..cursor];
    let opening_end = find_tag_opening_end(source, cursor)?;
    let props = &source[cursor..opening_end];
    let self_closing = props.trim_end().ends_with('/') || is_void_tag(tag_name);

    if self_closing {
        return Some((tag_name, opening_end + 1, opening_end + 1, props, ""));
    }

    let content_start = opening_end + 1;
    let element_end = find_matching_tag_end(source, content_start, tag_name)?;
    let close_start = element_end.checked_sub(tag_name.len() + 3)?;
    if close_start < content_start {
        return None;
    }

    Some((
        tag_name,
        opening_end + 1,
        element_end,
        props,
        &source[content_start..close_start],
    ))
}

/// Byte-indexed prop parser. Handles both quoted and unquoted values,
pub fn parse_props_v7(raw: &str) -> HashMap<String, Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return HashMap::new();
    }

    // Fast path: no '=' means every token is a boolean flag.
    if !trimmed.contains('=') {
        let mut map = HashMap::new();
        for token in trimmed.split_whitespace() {
            let key = token.trim_end_matches('/');
            if !key.is_empty() {
                map.insert(key.to_string(), Value::Bool(true));
            }
        }
        return map;
    }

    let chars: Vec<char> = trimmed.chars().collect();
    let mut map = HashMap::new();
    let mut index = 0usize;
    let len = chars.len();

    while index < len {
        while index < len && chars[index].is_whitespace() {
            index += 1;
        }
        if index >= len || chars[index] == '/' {
            break;
        }

        let mut key = String::new();
        while index < len
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

        while index < len && chars[index].is_whitespace() {
            index += 1;
        }

        if index >= len || chars[index] != '=' {
            map.insert(key, Value::Bool(true));
            continue;
        }

        index += 1;
        while index < len && chars[index].is_whitespace() {
            index += 1;
        }

        if index >= len {
            map.insert(key, Value::Bool(true));
            break;
        }

        let quote = chars[index];

        if quote != '"' && quote != '\'' {
            let mut value = String::new();
            while index < len
                && !chars[index].is_whitespace()
                && chars[index] != '/'
            {
                value.push(chars[index]);
                index += 1;
            }
            let parsed = serde_json::from_str::<Value>(&value)
                .unwrap_or_else(|_| Value::String(value));
            map.insert(key, parsed);
            continue;
        }

        index += 1;
        let value = parse_quoted_prop_value(&chars, &mut index, quote);
        let parsed = serde_json::from_str::<Value>(&value)
            .unwrap_or_else(|_| Value::String(value));
        map.insert(key, parsed);
    }

    map
}

fn scan_nested_tag(chars: &[char], start: usize) -> Option<(usize, i8)> {
    if chars.get(start) != Some(&'<') {
        return None;
    }

    let mut index = start + 1;
    if index >= chars.len() {
        return None;
    }

    let closing = chars[index] == '/';
    if closing {
        index += 1;
    }

    if index >= chars.len() {
        return None;
    }

    let first = chars[index];
    if first == '!' || first == '?' || first.is_whitespace() {
        return None;
    }

    while index < chars.len() {
        let current = chars[index];

        if current == '"' || current == '\'' {
            let quote = current;
            index += 1;
            while index < chars.len() {
                if chars[index] == quote {
                    index += 1;
                    break;
                }
                index += 1;
            }
            continue;
        }

        if current == '>' {
            let mut previous = index;
            while previous > start && chars[previous - 1].is_whitespace() {
                previous -= 1;
            }
            let self_closing = !closing && previous > start && chars[previous - 1] == '/';
            let kind = if closing {
                -1
            } else if self_closing {
                0
            } else {
                1
            };
            return Some((index, kind));
        }
        index += 1;
    }

    None
}

// Kept as a shim for compatibility with the previous signature.
#[allow(dead_code)]
fn parse_quoted_prop_value(
    chars: &[char],
    index: &mut usize,
    outer_quote: char,
) -> String {
    let mut value = String::new();
    let mut nested_depth: usize = 0;

    while *index < chars.len() {
        let current = chars[*index];

        if current == '<' {
            if let Some((end, kind)) = scan_nested_tag(chars, *index) {
                value.extend(chars[*index..=end].iter().copied());
                match kind {
                    1 => nested_depth += 1,
                    -1 => {
                        if nested_depth > 0 {
                            nested_depth -= 1;
                        }
                    }
                    _ => {}
                }
                *index = end + 1;
                continue;
            }
        }

        if current == outer_quote && nested_depth == 0 {
            *index += 1;
            break;
        }

        value.push(current);
        *index += 1;
    }

    value
}

fn apply_active_nav_class(html: &str, context: &RenderedPage) -> String {
    let Some(Value::String(current_path)) = context.template_context.get("@path") else {
        return html.to_string();
    };

    // Fast path: no anchor tags means nothing to mark active.
    if !html.contains("<a ") {
        return html.to_string();
    }

    static NAV_LINK_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)<a\s+([^>]*?)>"#).unwrap()
    });
    static NAV_HREF_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)href\s*=\s*["']([^"']+)["']"#).unwrap()
    });
    static NAV_CLASS_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)class\s*=\s*["']([^"']*)["']"#).unwrap()
    });

    NAV_LINK_RE
        .replace_all(html, |caps: &regex::Captures| {
            let full_match = caps.get(0).unwrap().as_str();
            let attrs = caps.get(1).unwrap().as_str();

            let Some(href_cap) = NAV_HREF_RE.captures(attrs) else {
                return full_match.to_string();
            };
            let href = href_cap.get(1).unwrap().as_str();

            // `current_path.starts_with("{href}/")` without building the string
            let is_match = href == current_path
                || (current_path != "/"
                    && current_path
                        .strip_prefix(href)
                        .map_or(false, |rest| rest.starts_with('/')));

            if !is_match {
                return full_match.to_string();
            }

            if let Some(class_cap) = NAV_CLASS_RE.captures(attrs) {
                let existing = class_cap.get(1).unwrap().as_str();
                if existing.split_whitespace().any(|c| c == "active") {
                    return full_match.to_string();
                }
                let new_class = format!("{} active", existing.trim());
                let quote = if class_cap.get(0).unwrap().as_str().contains('\'') {
                    '\''
                } else {
                    '"'
                };
                let new_attrs =
                    NAV_CLASS_RE.replace(attrs, &format!("class={quote}{}{quote}", new_class));
                format!("<a {}>", new_attrs)
            } else {
                format!("<a {} class=\"active\">", attrs.trim())
            }
        })
        .into_owned()
}

/// Props that carry markup or template placeholders were authored in the
/// template source, so they are trusted as a fragment: nested `{{...}}` is
/// resolved (and escaped) by the interpolation pass that follows.
fn prop_value_for_context(key: &str, value: &Value) -> Value {
    if let Value::String(s) = value {
        if key == "attributes" || s.contains('<') || s.contains("{{") {
            return Value::String(mark_safe_html(s.clone()));
        }
    }
    value.clone()
}

/// Same pipeline as `render_component_template`, but renders inside `ctx` with
/// `props` + `children` overlaid, then restores `ctx` exactly as it was.
fn render_component_template_scoped(
    template: &str,
    ctx: &mut HashMap<String, Value>,
    props: &HashMap<String, Value>,
    children: String,
) -> String {
    let mut saved: Vec<(&str, Option<Value>)> = Vec::with_capacity(props.len() + 1);
    for (key, value) in props {
        let old = ctx.insert(key.clone(), prop_value_for_context(key, value));
        saved.push((key.as_str(), old));
    }
    let old = ctx.insert("children".to_string(), Value::String(mark_safe_html(children)));
    saved.push(("children", old));

    let rendered = render_control_flow_in_scope(template, ctx);
    let rendered = render_interpolations(&rendered, ctx);
    let cleaned = clean_empty_tags(&rendered);
    let out = normalize_class_attributes(&cleaned);

    // Restore in reverse so a prop literally named "children" unwinds correctly.
    for (key, old) in saved.into_iter().rev() {
        match old {
            Some(value) => {
                ctx.insert(key.to_string(), value);
            }
            None => {
                ctx.remove(key);
            }
        }
    }
    out
}

pub fn normalize_class_attributes(html: &str) -> String {
    if !html.contains("class") {
        return html.to_string();
    }
    CLASS_RE
        .replace_all(html, |captures: &regex::Captures| {
            let value = captures
                .get(2)
                .or_else(|| captures.get(3))
                .map(|value| value.as_str())
                .unwrap_or("");

            let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");

            let quote = if captures
                .get(1)
                .map(|value| value.as_str())
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

fn attributes_from_meta(meta: &TemplateMeta, props: &HashMap<String, Value>) -> String {
    let used = &meta.used;
    let mut attributes = Vec::with_capacity(props.len());

    if meta.has_button && !props.contains_key("type") && !used.contains("type") {
        attributes.push("type=\"button\"".to_string());
    }

    for (key, value) in props {
        // 🔒 Skip framework-consumed props + component internals
        if matches!(key.as_str(),
            "children" | "attributes" | "class" |
            "auth" | "guest" | "roles" | "role" |
            "cache" |
            "footer" | "navbar" | "hero"
        ) {
            continue;
        }
        if used.contains(key) {
            continue;
        }

        if is_boolean_attribute(key) {
            if let Value::Bool(value) = value {
                if *value {
                    attributes.push(key.clone());
                }
            } else if let Value::String(value) = value {
                if value.eq_ignore_ascii_case("true") || value == key {
                    attributes.push(key.clone());
                }
            }
            continue;
        }

        if let Value::String(value) = value {
            if value.trim().is_empty() {
                continue;
            }
            attributes.push(format!("{}=\"{}\"", key, escape_html_attribute(value)));
        }
    }

    attributes.sort();
    attributes.join(" ")
}

const BOOLEAN_ATTRS: &[&str] = &[
    "allowfullscreen", "async", "autofocus", "autoplay", "checked", "controls", "default",
    "defer", "disabled", "formnovalidate", "hidden", "inert", "ismap", "itemscope", "loop",
    "multiple", "muted", "nomodule", "novalidate", "open", "playsinline", "readonly",
    "required", "reversed", "selected",
];

pub fn is_boolean_attribute(name: &str) -> bool {
    BOOLEAN_ATTRS.iter().any(|attr| name.eq_ignore_ascii_case(attr))
}

pub fn render_nested_vlo_content(content: &str, context: &mut RenderedPage) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut source = strip_server_block(content);

    for _ in 0..20 {
        let layout = render_tag_opt(&source, "BaseLayout", context);
        let components = {
            let current = layout.as_deref().unwrap_or(&source);
            render_components_opt(current, context)
        };

        // Nothing was rendered → the text is unchanged: done (no copy, no compare).
        let next_source = match (components, layout) {
            (Some(rendered), _) => rendered,
            (None, Some(rendered)) => rendered,
            (None, None) => break,
        };

        if next_source == source {
            break;
        }
        source = next_source;
    }

    source
}