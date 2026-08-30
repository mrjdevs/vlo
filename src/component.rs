use crate::{
    api::strip_server_block,
    state::{
        RenderedPage, CompiledTemplate, CLASS_RE, ELEMENT_RE, SLOT_RE, STYLE_RE, TEMPLATE_CACHE,
    },
    template::{render_control_flow, render_interpolations},
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

// ============================================================
// 14. VLO COMPONENT SYSTEM
// ============================================================

pub fn component_path(name: &str) -> Option<PathBuf> {
    let root = crate::state::get_project_root();

    let layout = root
        .join("layouts")
        .join(format!("{}.vlo", name));

    if layout.exists() {
        return Some(layout);
    }

    let component = root
        .join("components")
        .join(format!("{}.vlo", name));

    if component.exists() {
        return Some(component);
    }

    None
}

pub fn read_component_template(
    path: &Path,
) -> Option<Arc<CompiledTemplate>> {
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

pub fn render_tag(
    source: &str,
    tag: &str,
    context: &mut RenderedPage,
) -> String {
    if let Some((start, end, props, children)) = find_tag(source, tag) {
        return format!(
            "{}{}{}",
            &source[..start],
            render_component_file(tag, &props, &children, context),
            &source[end..]
        );
    }

    source.to_string()
}

pub fn render_components(
    source: &str,
    context: &mut RenderedPage,
) -> String {
    let mut output = String::new();
    let mut last = 0;
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut index = 0;

    while index < chars.len() {
        let (position, character) = chars[index];

        if character == '<'
            && index + 1 < chars.len()
            && chars[index + 1].1.is_ascii_uppercase()
        {
            let mut end = index + 1;
            while end < chars.len()
                && (chars[end].1.is_ascii_alphanumeric() || chars[end].1 == '_')
            {
                end += 1;
            }

            let tag = &source[chars[index + 1].0..chars[end].0];

            if let Some((_, tag_end, props, children)) =
                find_tag(&source[position..], tag)
            {
                output.push_str(&source[last..position]);
                output.push_str(&render_component_file(
                    tag, &props, &children, context,
                ));
                last = position + tag_end;

                while index < chars.len() && chars[index].0 < last {
                    index += 1;
                }
                continue;
            }
        }

        index += 1;
    }

    output.push_str(&source[last..]);
    output
}

pub fn find_tag(
    source: &str,
    name: &str,
) -> Option<(usize, usize, String, String)> {
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

        index += remaining
            .chars()
            .next()
            .map(|ch| ch.len_utf8())
            .unwrap_or(1);
    }

    None
}

pub fn render_component_file(
    name: &str,
    props_str: &str,
    children: &str,
    context: &mut RenderedPage,
) -> String {
    let path = match component_path(name) {
        Some(path) => path,
        None => return format!("<!-- Missing component: {} -->", name),
    };

    let compiled = match read_component_template(&path) {
        Some(template) => template,
        None => return format!("<!-- Missing template: {} -->", name),
    };

    if !compiled.css.trim().is_empty() {
        context.add_style(name, compiled.css.trim());
    }

    let template = &compiled.template;
    let props = parse_props_v7(props_str);
    let (raw_named_slots, raw_default_slot) = parse_slot_content(children);

    let mut named_slots = HashMap::new();
    for (slot_name, slot_content) in raw_named_slots {
        let rendered = render_nested_vlo_content(&slot_content, context);
        named_slots.insert(slot_name, rendered);
    }

    let default_slot = render_nested_vlo_content(&raw_default_slot, context);
    let mut render_ctx = props.clone();
    render_ctx.insert("children".to_string(), Value::String(default_slot.clone()));

    let rendered = render_component_template(template, &render_ctx);
    let rendered = render_slots(&rendered, &named_slots, &default_slot);

    let incoming_class = props.get("class").and_then(|v| {
        if let Value::String(s) = v {
            Some(s.clone())
        } else {
            None
        }
    });

    let attributes = build_component_attributes(template, &props);
    if attributes.is_empty() && incoming_class.is_none() {
        return rendered;
    }

    let skip_check = STYLE_RE.replace_all(&rendered, "");
    if let Some(first_tag) = ELEMENT_RE
        .captures(&skip_check)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
    {
        if first_tag.eq_ignore_ascii_case("style")
            || first_tag.eq_ignore_ascii_case("script")
        {
            return rendered;
        }
    }

    if let Some(captures) = ELEMENT_RE.captures(&rendered) {
        let full_match = captures.get(0).unwrap();
        let tag_name = captures.get(1).unwrap().as_str();
        let existing_attributes = captures
            .get(2)
            .map(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let (existing_attributes, class_attr) = if let Some(extra) = incoming_class
            .as_deref()
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
        {
            if let Some(m) = CLASS_RE.captures(&existing_attributes) {
                let existing_value = m
                    .get(2)
                    .or_else(|| m.get(3))
                    .map(|v| v.as_str())
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
            Some(c) if attributes.is_empty() => c,
            Some(c) => format!("{} {}", c, attributes),
            None => attributes,
        };

        if attributes.is_empty() {
            return rendered;
        }

        let replacement = if existing_attributes.trim().is_empty() {
            format!("<{} {}>", tag_name, attributes)
        } else {
            format!(
                "<{} {} {}>",
                tag_name,
                existing_attributes.trim(),
                attributes
            )
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

// ============================================================
// 15. VLO SLOT SYSTEM
// ============================================================

pub fn render_slots(
    template: &str,
    named_slots: &HashMap<String, String>,
    default_slot: &str,
) -> String {
    let res = SLOT_RE
        .replace_all(template, |captures: &regex::Captures| {
            let name = captures
                .get(1)
                .map(|v| v.as_str().trim())
                .unwrap_or("");
            let fallback = captures.get(2).map(|v| v.as_str()).unwrap_or("");

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

    SLOT_RE.replace_all(&res, "").into_owned()
}

pub fn parse_slot_content(
    children: &str,
) -> (HashMap<String, String>, String) {
    let mut named_slots = HashMap::new();
    let mut default_content = String::new();
    let mut cursor = 0;
    let mut default_start = 0;

    while cursor < children.len() {
        let remaining = &children[cursor..];
        let open_start = match remaining.find('<') {
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

        let (_tag_name, _opening_end, element_end, props, content) = tag_info;

        if let Some(slot_name) = get_slot_name(&props) {
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

        cursor = element_end;
    }

    if default_start < children.len() {
        default_content.push_str(&children[default_start..]);
    }

    (named_slots, default_content)
}

pub fn get_slot_name(props: &str) -> Option<String> {
    parse_props_v7(props).get("slot").and_then(|v| {
        if let Value::String(s) = v {
            Some(s.clone())
        } else {
            None
        }
    })
}

pub fn is_void_tag(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

pub fn parse_element_at(
    source: &str,
    start: usize,
) -> Option<(String, usize, usize, String, &str)> {
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
        if ch.is_ascii_alphanumeric()
            || ch == '-'
            || ch == '_'
            || ch == ':'
        {
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

    let self_closing =
        props.trim_end().ends_with('/') || is_void_tag(&tag_name);

    if self_closing {
        return Some((tag_name, opening_end + 1, opening_end + 1, props, ""));
    }

    let content_start = opening_end + 1;
    let element_end = find_matching_tag_end(source, content_start, &tag_name)?;

    let close_start = element_end
        .checked_sub(format!("</{}>", tag_name).len())?;

    if close_start < content_start {
        return None;
    }

    let content = &source[content_start..close_start];
    Some((tag_name, opening_end + 1, element_end, props, content))
}

pub fn find_tag_opening_end(source: &str, start: usize) -> Option<usize> {
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

pub fn find_matching_tag_end(
    source: &str,
    start: usize,
    tag_name: &str,
) -> Option<usize> {
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

        cursor += remaining
            .chars()
            .next()
            .map(|ch| ch.len_utf8())
            .unwrap_or(1);
    }

    None
}

// ============================================================
// 16. VLO PROP SYSTEM
// ============================================================

pub fn parse_props_v7(raw: &str) -> HashMap<String, Value> {
    let chars: Vec<char> = raw
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .chars()
        .collect();

    let mut map = HashMap::new();
    let mut index = 0;

    while index < chars.len() {
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }

        if index >= chars.len() || chars[index] == '/' {
            break;
        }

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

        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }

        if index < chars.len() && chars[index] == '=' {
            index += 1;
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }

            if index >= chars.len() {
                map.insert(key, Value::Bool(true));
                break;
            }

            let quote = chars[index];
            let mut value = String::new();

            if quote == '"' || quote == '\'' {
                index += 1;
                while index < chars.len() && chars[index] != quote {
                    value.push(chars[index]);
                    index += 1;
                }
                if index < chars.len() {
                    index += 1;
                }
            } else {
                while index < chars.len()
                    && !chars[index].is_whitespace()
                    && chars[index] != '/'
                {
                    value.push(chars[index]);
                    index += 1;
                }
            }

            map.insert(key, Value::String(value));
        } else {
            map.insert(key, Value::Bool(true));
        }
    }

    map
}

pub fn render_component_template(
    template: &str,
    props: &HashMap<String, Value>,
) -> String {
    let rendered = render_control_flow(template, props);
    let rendered = render_interpolations(&rendered, props);
    let cleaned = clean_empty_tags(&rendered);
    normalize_class_attributes(&cleaned)
}

pub fn normalize_class_attributes(html: &str) -> String {
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

pub fn build_component_attributes(
    template: &str,
    props: &HashMap<String, Value>,
) -> String {
    let mut used = HashSet::new();
    for captures in crate::state::PROP_RE.captures_iter(template) {
        used.insert(captures[1].to_string());
    }

    let mut attributes = Vec::new();
    let button_default = template.to_ascii_lowercase().contains("<button");

    if button_default && !props.contains_key("type") && !used.contains("type") {
        attributes.push("type=\"button\"".to_string());
    }

    for (key, value) in props {
        if key == "children" || key == "attributes" || key == "class" {
            continue;
        }
        if used.contains(key) {
            continue;
        }
        if is_boolean_attribute(key) {
            if let Value::Bool(b) = value {
                if *b {
                    attributes.push(key.clone());
                }
            } else if let Value::String(s) = value {
                if s.eq_ignore_ascii_case("true") || s == key {
                    attributes.push(key.clone());
                }
            }
            continue;
        }
        if let Value::String(s) = value {
            if s.trim().is_empty() {
                continue;
            }
            attributes.push(format!("{}=\"{}\"", key, escape_html_attribute(s)));
        }
    }

    attributes.sort();
    attributes.join(" ")
}

pub fn is_boolean_attribute(name: &str) -> bool {
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

pub fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn render_nested_vlo_content(
    content: &str,
    context: &mut RenderedPage,
) -> String {
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
// 19. VLO STYLE & EMPTY TAG ENGINE
// ============================================================

pub fn strip_blank_lines(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    for line in html.lines() {
        if !line.trim().is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

pub fn clean_empty_tags(html: &str) -> String {
    let mut cleaned = html.to_string();
    loop {
        let result = crate::state::EMPTY_TAG_RE.replace_all(&cleaned, "");
        if result == cleaned {
            break;
        }
        cleaned = result.to_string();
    }
    cleaned
}