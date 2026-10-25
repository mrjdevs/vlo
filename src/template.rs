use crate::state::PROP_RE;
use serde_json::Value;
use std::{
    borrow::Cow,
    cell::OnceCell,
    collections::HashMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, RwLock},
    time::SystemTime,
};

static RUNTIME_DATA_SOURCE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)data-source\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    )
    .unwrap()
});

/// Prefix marking a string that is already rendered, trusted HTML. The
/// interpolator strips it and emits the payload unescaped. `|raw` / `|safe`
/// provide the same opt-in for plain values.
pub const SAFE_HTML_MARKER: &str = "\u{1}__VLO_SAFE_HTML__";

pub fn mark_safe_html(value: String) -> String {
    if value.is_empty() || value.starts_with(SAFE_HTML_MARKER) {
        value
    } else {
        let mut out = String::with_capacity(SAFE_HTML_MARKER.len() + value.len());
        out.push_str(SAFE_HTML_MARKER);
        out.push_str(&value);
        out
    }
}

#[inline]
fn strip_safe_markers(value: &str) -> Cow<'_, str> {
    if value.contains(SAFE_HTML_MARKER) {
        Cow::Owned(value.replace(SAFE_HTML_MARKER, ""))
    } else {
        Cow::Borrowed(value)
    }
}

#[inline]
fn strip_safe_prefix(value: &str) -> (&str, bool) {
    match value.strip_prefix(SAFE_HTML_MARKER) {
        Some(rest) => (rest, true),
        None => (value, false),
    }
}

fn resolve_operand_stripped(operand: &str, context: &HashMap<String, Value>) -> Value {
    match resolve_operand(operand, context) {
        Value::String(value) => match value.strip_prefix(SAFE_HTML_MARKER) {
            Some(rest) => Value::String(strip_safe_markers(rest).into_owned()),
            None => Value::String(value),
        },
        other => other,
    }
}

// ═════════════════════════════════════════════════════════════
// Quote mask — one linear byte pass
//
// mask[pos] == "pos is inside an attribute quote". Only ever queried at `{{`
// (ASCII) positions, so scanning bytes instead of chars is equivalent: UTF-8
// continuation bytes can never equal `<`, `>`, `"` or `'`.
//
// Perf: text outside tags is skipped with one byte search for the next `<`, and
// <script>/<style> bodies are skipped with one substring search.
// ═════════════════════════════════════════════════════════════
fn quote_mask(template: &str) -> Vec<bool> {
    let bytes = template.as_bytes();
    let len = bytes.len();
    let mut mask = vec![false; len + 1];

    let mut in_tag = false;
    let mut quote: u8 = 0; // 0 = none
    let mut i = 0usize;

    while i < len {
        if !in_tag {
            // Outside a tag every mask bit is false: jump to the next '<'.
            match bytes[i..].iter().position(|&b| b == b'<') {
                Some(p) => i += p,
                None => break,
            }
            let rest = &template[i..];
            if rest.starts_with("<script") || rest.starts_with("<style") {
                let close = if rest.starts_with("<script") { "</script>" } else { "</style>" };
                // skip the '<', then jump past the closing tag (or to the end)
                i += 1;
                match template[i..].find(close) {
                    Some(p) => i += p + close.len(),
                    None => i = len,
                }
                continue;
            }
            in_tag = true;
            i += 1;
            continue;
        }

        // Inside a tag
        mask[i] = quote != 0;
        let b = bytes[i];
        if quote != 0 {
            if b == quote {
                quote = 0;
            }
        } else {
            match b {
                b'"' | b'\'' => quote = b,
                b'>' => in_tag = false,
                _ => {}
            }
        }
        i += 1;
    }

    mask[len] = in_tag && quote != 0;
    mask
}

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Find `target` (ASCII) outside of quotes. Skips escaped closing quotes.
fn find_unquoted_char(s: &str, target: char) -> Option<usize> {
    debug_assert!(target.is_ascii());
    let t = target as u8;
    let bytes = s.as_bytes();
    let mut quote: u8 = 0;

    for (i, &b) in bytes.iter().enumerate() {
        if quote != 0 {
            if b == quote && (i == 0 || bytes[i - 1] != b'\\') {
                quote = 0;
            }
        } else if b == b'"' || b == b'\'' {
            quote = b;
        } else if b == t {
            return Some(i);
        }
    }
    None
}

// ═════════════════════════════════════════════════════════════
// Value lookup (borrowing — no deep clones)
// ═════════════════════════════════════════════════════════════
fn get_nested_ref<'a>(path: &str, context: &'a HashMap<String, Value>) -> Option<&'a Value> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let mut parts = path.split('.');
    let mut current = context.get(parts.next()?)?;
    for part in parts {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(arr) => arr.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}


// ═════════════════════════════════════════════════════════════
// Expression language
// ═════════════════════════════════════════════════════════════
pub fn evaluate_condition(expr: &str, context: &HashMap<String, Value>) -> bool {
    let expr = expr.trim();
    if expr.is_empty() {
        return false;
    }

    // Fast path: a bare variable/literal (`item.active`, `logged_in`, `5`).
    // No operators, quotes, parens, whitespace, and no word operator (`in`,
    // `contains`) possible → result is just the truthiness of the operand.
    if !expr
        .bytes()
        .any(|b| matches!(b, b'|' | b'&' | b'(' | b')' | b'!' | b'=' | b'<' | b'>' | b'"' | b'\'' | b' ' | b'\t' | b'\n' | b'\r'))
        && (!expr.contains("in")
            || (find_word_operator(expr, "in").is_none()
                && find_word_operator(expr, "contains").is_none()))
    {
        return is_truthy(&resolve_operand_cow(expr, context));
    }

    evaluate_or(expr, context)
}

fn evaluate_or(expr: &str, context: &HashMap<String, Value>) -> bool {
    if !expr.contains("||") {
        return evaluate_and(expr.trim(), context);
    }
    for branch in split_logical(expr, "||") {
        if evaluate_and(branch.trim(), context) {
            return true;
        }
    }
    false
}

fn evaluate_and(expr: &str, context: &HashMap<String, Value>) -> bool {
    if !expr.contains("&&") {
        return evaluate_atom(expr.trim(), context);
    }
    for branch in split_logical(expr, "&&") {
        if !evaluate_atom(branch.trim(), context) {
            return false;
        }
    }
    true
}

fn evaluate_atom(expr: &str, context: &HashMap<String, Value>) -> bool {
    let expr = expr.trim();

    if expr.starts_with('(') && expr.ends_with(')') {
        let mut depth = 0i32;
        let mut wraps_all = true;
        for (i, ch) in expr.char_indices() {
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 && i < expr.len() - 1 {
                    wraps_all = false;
                    break;
                }
            }
        }
        if wraps_all && depth == 0 {
            return evaluate_condition(&expr[1..expr.len() - 1], context);
        }
    }

    if let Some(stripped) = expr.strip_prefix('!') {
        return !evaluate_atom(stripped.trim(), context);
    }

    // Word operators ("in" is a substring of every one of them → one cheap gate).
    if expr.contains("in") {
        for op in ["not in", "in", "contains"] {
            if let Some(pos) = find_word_operator(expr, op) {
                let left = resolve_operand_cow(&expr[..pos], context);
                let right = resolve_operand_cow(&expr[pos + op.len()..], context);
                return match op {
                    "in" => value_in(&left, &right),
                    "not in" => !value_in(&left, &right),
                    "contains" => value_in(&right, &left),
                    _ => false,
                };
            }
        }
    }

    // Comparison operators: skip the six scans when no candidate byte exists.
    if expr.bytes().any(|b| matches!(b, b'=' | b'<' | b'>')) {
        for op in ["==", "!=", "<=", ">=", "<", ">"] {
            if let Some(pos) = find_operator(expr, op) {
                let left = resolve_operand_cow(&expr[..pos], context);
                let right = resolve_operand_cow(&expr[pos + op.len()..], context);
                return compare_values(&left, &right, op);
            }
        }
    }

    is_truthy(&resolve_operand_cow(expr, context))
}

fn find_word_operator(expr: &str, op: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_quotes: Option<char> = None;
    let bytes = expr.as_bytes();
    let op_bytes = op.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let ch = bytes[i] as char;
        if let Some(q) = in_quotes {
            if ch == q && (i == 0 || bytes[i - 1] != b'\\') {
                in_quotes = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_quotes = Some(ch);
            i += 1;
            continue;
        }
        if ch == '(' {
            depth += 1;
            i += 1;
            continue;
        }
        if ch == ')' {
            depth -= 1;
            i += 1;
            continue;
        }

        if depth == 0
            && i + op_bytes.len() <= bytes.len()
            && &bytes[i..i + op_bytes.len()] == op_bytes
        {
            let before_ok = i == 0 || !is_word_byte(bytes[i - 1]);
            let after_idx = i + op_bytes.len();
            let after_ok = after_idx == bytes.len() || !is_word_byte(bytes[after_idx]);
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn value_in(needle: &Value, haystack: &Value) -> bool {
    match haystack {
        Value::Array(arr) => arr.contains(needle),
        Value::String(s) => needle.as_str().map(|n| s.contains(n)).unwrap_or(false),
        Value::Object(obj) => needle.as_str().map(|n| obj.contains_key(n)).unwrap_or(false),
        _ => false,
    }
}

fn split_logical<'a>(expr: &'a str, op: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quotes: Option<char> = None;
    let mut start = 0;
    let bytes = expr.as_bytes();
    let op_bytes = op.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let ch = bytes[i] as char;
        if let Some(q) = in_quotes {
            if ch == q && (i == 0 || bytes[i - 1] != b'\\') {
                in_quotes = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_quotes = Some(ch);
            i += 1;
            continue;
        }
        if ch == '(' {
            depth += 1;
            i += 1;
            continue;
        }
        if ch == ')' {
            depth -= 1;
            i += 1;
            continue;
        }

        if depth == 0
            && i + op_bytes.len() <= bytes.len()
            && &bytes[i..i + op_bytes.len()] == op_bytes
        {
            parts.push(&expr[start..i]);
            start = i + op_bytes.len();
            i = start;
            continue;
        }
        i += 1;
    }
    parts.push(&expr[start..]);
    parts
}

fn find_operator(expr: &str, op: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_quotes: Option<char> = None;
    let bytes = expr.as_bytes();
    let op_bytes = op.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let ch = bytes[i] as char;
        if let Some(q) = in_quotes {
            if ch == q && (i == 0 || bytes[i - 1] != b'\\') {
                in_quotes = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_quotes = Some(ch);
            i += 1;
            continue;
        }
        if ch == '(' {
            depth += 1;
            i += 1;
            continue;
        }
        if ch == ')' {
            depth -= 1;
            i += 1;
            continue;
        }

        if depth == 0
            && i + op_bytes.len() <= bytes.len()
            && &bytes[i..i + op_bytes.len()] == op_bytes
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Resolves an operand without cloning when it is a variable lookup.
fn resolve_operand_cow<'a>(operand: &str, context: &'a HashMap<String, Value>) -> Cow<'a, Value> {
    let trimmed = operand.trim();

    match trimmed {
        "true" => return Cow::Owned(Value::Bool(true)),
        "false" => return Cow::Owned(Value::Bool(false)),
        "null" | "nil" => return Cow::Owned(Value::Null),
        _ => {}
    }

    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return Cow::Owned(Value::String(trimmed[1..trimmed.len() - 1].to_string()));
    }

    // Only strings that can start a number (incl. inf / nan spellings) are parsed.
    if let Some(&b0) = bytes.first() {
        if b0.is_ascii_digit() || matches!(b0, b'-' | b'+' | b'.' | b'i' | b'I' | b'n' | b'N') {
            if let Ok(num) = trimmed.parse::<i64>() {
                return Cow::Owned(Value::Number(num.into()));
            }
            if let Ok(num) = trimmed.parse::<f64>() {
                return Cow::Owned(
                    serde_json::Number::from_f64(num)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                );
            }
        }
    }

    match get_nested_ref(trimmed, context) {
        Some(v) => Cow::Borrowed(v),
        None => Cow::Owned(Value::Null),
    }
}

fn resolve_operand(operand: &str, context: &HashMap<String, Value>) -> Value {
    resolve_operand_cow(operand, context).into_owned()
}

fn is_literal_operand(base: &str) -> bool {
    let b = base.trim();
    if b.starts_with('"') || b.starts_with('\'') {
        return true;
    }
    matches!(b, "true" | "false" | "null" | "nil") || b.parse::<f64>().is_ok()
}

fn compare_values(left: &Value, right: &Value, op: &str) -> bool {
    match (left, right) {
        (Value::Number(l), Value::Number(r)) => {
            let l = l.as_f64().unwrap_or(0.0);
            let r = r.as_f64().unwrap_or(0.0);
            match op {
                "==" => l == r,
                "!=" => l != r,
                "<" => l < r,
                ">" => l > r,
                "<=" => l <= r,
                ">=" => l >= r,
                _ => false,
            }
        }
        (Value::String(l), Value::String(r)) => match op {
            "==" => l == r,
            "!=" => l != r,
            "<" => l < r,
            ">" => l > r,
            "<=" => l <= r,
            ">=" => l >= r,
            _ => false,
        },
        (Value::Bool(l), Value::Bool(r)) => match op {
            "==" => l == r,
            "!=" => l != r,
            _ => false,
        },
        _ => match op {
            "==" => left == right,
            "!=" => left != right,
            _ => false,
        },
    }
}

pub fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

// ═════════════════════════════════════════════════════════════
// Token scanner — single-brace blocks only [G1]
//
// Perf: jumps straight to the next `{` / `<` byte instead of testing every
// byte, then compares only against that bucket's tokens.
// ═════════════════════════════════════════════════════════════
const BRACE_TOKENS: &[&str] = &[
    "{{", "{if ", "{for ", "{unless ", "{switch ", "{with ",
    "{capture ", "{capture}", "{assign ", "{firstof ", "{include ",
    "{raw}", "{comment}", "{break}", "{continue}", "{else if ", "{else}",
    "{/if}", "{/for}", "{/unless}", "{/switch}", "{/with}", "{/capture}",
    "{/raw}", "{/comment}", "{case ", "{default}",
];
const ANGLE_TOKENS: &[&str] = &[
    "<script", "</script>", "<pre>", "<pre ", "<code>", "<code ",
    "<textarea>", "<textarea ",
];

pub fn find_next_token(template: &str, from: usize) -> Option<(usize, &'static str)> {
    let bytes = template.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        let p = bytes[i..].iter().position(|&b| b == b'{' || b == b'<')?;
        i += p;
        let rest = &bytes[i..];
        let bucket = if rest[0] == b'{' { BRACE_TOKENS } else { ANGLE_TOKENS };
        for &tok in bucket {
            if rest.starts_with(tok.as_bytes()) {
                return Some((i, tok));
            }
        }
        i += 1;
    }
    None
}

/// Extract the tag name from a token string.
fn tag_name_of(token: &str) -> &str {
    token
        .trim_start_matches('{')
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches('}')
}

// ═════════════════════════════════════════════════════════════
// Block end finder — single pass, zero allocations
// (the old version built 3 strings per call and re-ran 3 `find`s over the
//  remaining template on every nesting step)
// ═════════════════════════════════════════════════════════════
pub fn find_block_end(template: &str, start: usize, token: &str) -> Option<(usize, usize)> {
    let kind = tag_name_of(token);
    if kind.is_empty() {
        return None;
    }

    // [G2] Quote-aware header end.
    let header_offset = find_unquoted_char(&template[start..], '}')?;
    let header_end = start + header_offset + 1;

    let bytes = template.as_bytes();
    let kb = kind.as_bytes();
    let mut depth = 1usize;
    let mut cursor = header_end;

    while cursor < bytes.len() {
        let p = bytes[cursor..].iter().position(|&b| b == b'{')?;
        let pos = cursor + p;
        let after = &bytes[pos + 1..];

        // {/kind}
        if after.first() == Some(&b'/')
            && after[1..].starts_with(kb)
            && after.get(1 + kb.len()) == Some(&b'}')
        {
            depth -= 1;
            let end = pos + kb.len() + 3;
            if depth == 0 {
                return Some((header_end, end));
            }
            cursor = end;
            continue;
        }

        // [B3] `{kind ` or `{kind}` opens a nested block
        if after.starts_with(kb) && matches!(after.get(kb.len()), Some(b' ') | Some(b'}')) {
            depth += 1;
            cursor = pos + kb.len() + 2;
            continue;
        }

        cursor = pos + 1;
    }
    None
}

// ═════════════════════════════════════════════════════════════
// Branch splitting — one pass over `{` bytes [G1]
// ═════════════════════════════════════════════════════════════
const SPLIT_TOKENS: &[(&[u8], u8)] = &[
    (b"{if ", 0),
    (b"{for ", 0),
    (b"{unless ", 0),
    (b"{switch ", 0),
    (b"{with ", 0),
    (b"{capture ", 0),
    (b"{capture}", 0),
    (b"{/if}", 1),
    (b"{/for}", 1),
    (b"{/unless}", 1),
    (b"{/switch}", 1),
    (b"{/with}", 1),
    (b"{/capture}", 1),
    (b"{else if ", 2),
    (b"{else}", 3),
];

/// Borrowing version: no String allocations.
pub fn split_else_branches_ref(inner: &str) -> (&str, Option<&str>) {
    let bytes = inner.as_bytes();
    let mut depth = 0usize;
    let mut cursor = 0usize;

    while let Some(p) = bytes[cursor..].iter().position(|&b| b == b'{') {
        let pos = cursor + p;
        let rest = &bytes[pos..];

        let Some(&(pat, kind)) = SPLIT_TOKENS.iter().find(|(pat, _)| rest.starts_with(pat)) else {
            cursor = pos + 1;
            continue;
        };

        match kind {
            0 => depth += 1,
            1 => depth = depth.saturating_sub(1),
            _ if depth == 0 => return (&inner[..pos], Some(&inner[pos..])),
            _ => {}
        }
        cursor = pos + pat.len();
    }
    (inner, None)
}

// ═════════════════════════════════════════════════════════════
// Block evaluators
// ═════════════════════════════════════════════════════════════
pub fn evaluate_if_block(
    inner: &str,
    expression: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    if preserve_unresolved
        && expression_uses_runtime_query(expression)
        && !all_vars_resolved(expression, context)
    {
        return format!("{{if {}}}{}{{/if}}", expression, inner);
    }

    let (true_part, else_part) = split_else_branches_ref(inner);

    if evaluate_condition(expression, context) {
        return render_control_flow_internal(true_part, context, preserve_unresolved);
    }

    render_else_branch(else_part, context, preserve_unresolved)
}

pub fn evaluate_unless_block(
    inner: &str,
    expression: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let (true_part, else_part) = split_else_branches_ref(inner);
    if !evaluate_condition(expression, context) {
        return render_control_flow_internal(true_part, context, preserve_unresolved);
    }
    render_else_branch(else_part, context, preserve_unresolved)
}

/// Shared tail of `if` / `unless`: handles `{else if …}` chains and `{else}`.
fn render_else_branch(
    else_part: Option<&str>,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let Some(rest) = else_part else {
        return String::new();
    };
    if let Some(v) = rest.strip_prefix("{else if ") {
        if let Some(end) = v.find('}') {
            let expr = v[..end].trim();
            let body = &v[end + 1..];
            return evaluate_if_block(body, expr, context, preserve_unresolved);
        }
    }
    let else_body = rest.strip_prefix("{else}").unwrap_or(rest);
    render_control_flow_internal(else_body, context, preserve_unresolved)
}

// ═════════════════════════════════════════════════════════════
// Switch — nested-switch aware [B4]
// ═════════════════════════════════════════════════════════════
fn find_next_case_or_default(inner: &str, from: usize) -> Option<(usize, bool)> {
    let bytes = inner.as_bytes();
    let mut cursor = from;
    let mut switch_depth = 0usize;

    while let Some(p) = bytes.get(cursor..)?.iter().position(|&b| b == b'{') {
        cursor += p;
        let rest = &inner[cursor..];
        if rest.starts_with("{switch ") {
            switch_depth += 1;
            cursor += 8;
            continue;
        }
        if rest.starts_with("{/switch}") {
            if switch_depth == 0 {
                break;
            }
            switch_depth -= 1;
            cursor += 9;
            continue;
        }
        if switch_depth == 0 {
            if rest.starts_with("{case ") {
                return Some((cursor, false));
            }
            if rest.starts_with("{default") {
                return Some((cursor, true));
            }
        }
        cursor += 1;
    }
    None
}

pub fn evaluate_switch_block(
    inner: &str,
    subject_expr: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let subject = resolve_operand(subject_expr, context);
    let mut cursor = 0usize;

    while let Some((marker_pos, is_default)) = find_next_case_or_default(inner, cursor) {
        if is_default {
            let after = find_unquoted_char(&inner[marker_pos..], '}')
                .map(|p| marker_pos + p + 1)
                .unwrap_or(inner.len());
            let body_end =
                find_next_case_or_default(inner, after).map(|(p, _)| p).unwrap_or(inner.len());
            return render_control_flow_internal(&inner[after..body_end], context, preserve_unresolved);
        }

        let header_close = match find_unquoted_char(&inner[marker_pos..], '}') {
            Some(p) => marker_pos + p,
            None => break,
        };
        let case_val_expr = inner[marker_pos + 6..header_close].trim();
        let case_val = resolve_operand_cow(case_val_expr, context);
        let body_start = header_close + 1;
        let body_end = find_next_case_or_default(inner, body_start)
            .map(|(p, _)| p)
            .unwrap_or(inner.len());

        // exact match first, then case-insensitive fallback for strings
        let matched = compare_values(&subject, &case_val, "==")
            || match (&subject, &*case_val) {
                (Value::String(a), Value::String(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
                _ => false,
            };

        if matched {
            return render_control_flow_internal(&inner[body_start..body_end], context, preserve_unresolved);
        }
        cursor = body_end;
    }
    String::new()
}

// ═════════════════════════════════════════════════════════════
// For — [B1] break/continue content preservation
//
// Perf vs. the old version:
//  * the array is borrowed from the context (old: deep-cloned whole array)
//  * ONE context clone per loop (old: one deep clone of the entire context per
//    item). Loop variables are overwritten in place each iteration. A fresh
//    clone per iteration is only kept when the body can mutate the context
//    (`{assign}` / `{capture}`), so scoping behavior is unchanged.
//  * body quote-mask computed once per loop, not once per iteration
// ═════════════════════════════════════════════════════════════
const BREAK_MARKER: &str = "\u{0001}VLO_BREAK\u{0001}";
const CONTINUE_MARKER: &str = "\u{0001}VLO_CONTINUE\u{0001}";

#[inline]
fn set_var(map: &mut HashMap<String, Value>, key: &str, value: Value) {
    match map.get_mut(key) {
        Some(slot) => *slot = value,
        None => {
            map.insert(key.to_string(), value);
        }
    }
}

/// Public API kept as-is (borrowed context); clones once and delegates.

#[inline]
fn body_mutates_context(text: &str) -> bool {
    text.contains("{assign ") || text.contains("{capture")
}

/// Runs `f` with `vars` visible. If the text being rendered can write to the
/// context (`{assign}` / `{capture}`) a private clone is used, exactly like the
/// old code. Otherwise the vars are set in place and restored afterwards, which
/// avoids deep-cloning the whole context.
fn with_scope<R>(
    ctx: &mut HashMap<String, Value>,
    vars: Vec<(String, Value)>,
    mutates: bool,
    f: impl FnOnce(&mut HashMap<String, Value>) -> R,
) -> R {
    if mutates {
        let mut scoped = ctx.clone();
        for (k, v) in vars {
            scoped.insert(k, v);
        }
        return f(&mut scoped);
    }
    let mut saved: Vec<(String, Option<Value>)> = Vec::with_capacity(vars.len());
    for (k, v) in vars {
        let old = ctx.insert(k.clone(), v);
        saved.push((k, old));
    }
    let out = f(ctx);
    for (k, old) in saved.into_iter().rev() {
        match old {
            Some(v) => {
                ctx.insert(k, v);
            }
            None => {
                ctx.remove(&k);
            }
        }
    }
    out
}

fn for_block_scoped(
    inner: &str,
    expression: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let mut parts = expression.split_whitespace();
    let item_var = match parts.next() {
        Some(v) => v,
        None => return String::new(),
    };
    if parts.next() != Some("in") {
        return String::new();
    }

    let mut array_expr_parts: Vec<&str> = Vec::new();
    let mut limit: Option<usize> = None;
    let mut offset: Option<usize> = None;
    let mut reversed = false;

    for p in parts {
        if let Some(val) = p.strip_prefix("limit:") { limit = val.parse().ok(); }
        else if let Some(val) = p.strip_prefix("offset:") { offset = val.parse().ok(); }
        else if p == "reversed" { reversed = true; }
        else { array_expr_parts.push(p); }
    }

    let joined: String;
    let array_expr: &str = if array_expr_parts.len() == 1 {
        array_expr_parts[0]
    } else {
        joined = array_expr_parts.join(" ");
        &joined
    };

    let off = offset.unwrap_or(0);

    // Only the selected items are cloned (old: whole array, then again per item).
    let selected: Vec<Value> = {
        let looked = get_nested_ref(array_expr, context);

        // Auto-parse: strip quotes and parse JSON arrays stored as strings
        let parsed: Option<Value> = match looked {
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                let clean_s = if trimmed.len() >= 2
                    && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
                        || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
                {
                    &trimmed[1..trimmed.len() - 1]
                } else {
                    trimmed
                };
                if clean_s.starts_with('[') {
                    serde_json::from_str::<Value>(clean_s).ok()
                } else {
                    None
                }
            }
            _ => None,
        };

        match parsed.as_ref().or(looked) {
            Some(Value::Array(items)) => {
                let lim = limit.unwrap_or(items.len());
                if reversed {
                    items.iter().rev().skip(off).take(lim).cloned().collect()
                } else {
                    items.iter().skip(off).take(lim).cloned().collect()
                }
            }
            _ => Vec::new(),
        }
    };

    let (body, else_part) = split_else_branches_ref(inner);

    if selected.is_empty() {
        return render_else(else_part, context, preserve_unresolved);
    }

    let count = selected.len();
    let mut result = String::with_capacity(body.len() * count);

    let body_mutates = body_mutates_context(body);
    let body_mask: Option<Vec<bool>> = if body.contains("{{") { Some(quote_mask(body)) } else { None };

    // Loop variables are set in place and restored afterwards (unless the body
    // mutates the context, in which case each iteration gets a fresh clone).
    const LOOP_KEYS: [&str; 6] = ["@index", "@number", "@first", "@last", "@count", "@remaining"];
    let saved: Option<Vec<Option<Value>>> = if body_mutates {
        None
    } else {
        let mut v = Vec::with_capacity(7);
        v.push(context.get(item_var).cloned());
        for k in LOOP_KEYS {
            v.push(context.get(k).cloned());
        }
        Some(v)
    };

    let mut owned: Option<HashMap<String, Value>>;

    for (idx, item) in selected.into_iter().enumerate() {
        let scope: &mut HashMap<String, Value> = if body_mutates {
            owned = Some(context.clone());
            owned.as_mut().unwrap()
        } else {
            &mut *context
        };

        set_var(scope, item_var, item);
        set_var(scope, "@index", Value::Number(((idx + off) as i64).into()));
        set_var(scope, "@number", Value::Number(((idx + off + 1) as i64).into()));
        set_var(scope, "@first", Value::Bool(idx == 0));
        set_var(scope, "@last", Value::Bool(idx + 1 == count));
        set_var(scope, "@count", Value::Number((count as i64).into()));
        set_var(scope, "@remaining", Value::Number(((count - idx - 1) as i64).into()));

        let rendered = render_walk(body, body_mask.as_deref(), scope, preserve_unresolved);

        // Markers start with 0x01: skip the two searches when none is present.
        if !rendered.as_bytes().contains(&1) {
            result.push_str(&rendered);
            continue;
        }

        let break_pos = rendered.find(BREAK_MARKER);
        let continue_pos = rendered.find(CONTINUE_MARKER);

        match (break_pos, continue_pos) {
            (Some(bp), Some(cp)) if bp < cp => { result.push_str(&rendered[..bp]); break; }
            (Some(_), Some(cp)) => { result.push_str(&rendered[..cp]); continue; }
            (Some(bp), None) => { result.push_str(&rendered[..bp]); break; }
            (None, Some(cp)) => { result.push_str(&rendered[..cp]); continue; }
            (None, None) => result.push_str(&rendered),
        }
    }

    // Restore whatever the loop variables were before the loop.
    if let Some(saved) = saved {
        let mut it = saved.into_iter();
        let mut restore = |key: &str, old: Option<Value>| match old {
            Some(v) => { context.insert(key.to_string(), v); }
            None => { context.remove(key); }
        };
        restore(item_var, it.next().unwrap());
        for k in LOOP_KEYS {
            restore(k, it.next().unwrap());
        }
    }
    result
}

fn evaluate_assign(expr: &str, context: &mut HashMap<String, Value>) {
    let Some((name, val_expr)) = expr.split_once('=') else {
        return;
    };
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let val_expr = val_expr.trim();

    let value = if is_boolean_expression(val_expr) {
        Value::Bool(evaluate_condition(val_expr, context))
    } else {
        resolve_operand(val_expr, context)
    };

    context.insert(name.to_string(), value);
}

fn is_boolean_expression(expr: &str) -> bool {
    for op in ["&&", "||", "==", "!=", "<=", ">=", "<", ">"] {
        if find_operator(expr, op).is_some() {
            return true;
        }
    }
    for op in ["in", "not in", "contains"] {
        if find_word_operator(expr, op).is_some() {
            return true;
        }
    }
    expr.trim_start().starts_with('!')
}

fn evaluate_capture(
    inner: &str,
    name: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) {
    let rendered = render_control_flow_internal(inner, context, preserve_unresolved);
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        context.insert(trimmed.to_string(), Value::String(mark_safe_html(rendered)));
    }
}

fn parse_kv_pairs(raw: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut cursor = 0usize;
    let bytes = raw.as_bytes();

    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }
        let eq_pos = raw[cursor..]
            .find('=')
            .map(|p| cursor + p)
            .unwrap_or(raw.len());
        let key = raw[cursor..eq_pos].trim().to_string();
        if eq_pos >= raw.len() || key.is_empty() {
            break;
        }
        cursor = eq_pos + 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }
        if bytes[cursor] == b'"' || bytes[cursor] == b'\'' {
            let quote = bytes[cursor];
            let mut end_idx = cursor + 1;
            while end_idx < bytes.len() {
                if bytes[end_idx] == quote && bytes[end_idx - 1] != b'\\' {
                    break;
                }
                end_idx += 1;
            }
            let val = raw[cursor + 1..end_idx].to_string();
            pairs.push((key, val));
            cursor = end_idx + 1;
        } else {
            let val_end = raw[cursor..]
                .find(char::is_whitespace)
                .map(|p| cursor + p)
                .unwrap_or(raw.len());
            let val = raw[cursor..val_end].to_string();
            pairs.push((key, val));
            cursor = val_end;
        }
    }
    pairs
}

fn evaluate_with(
    inner: &str,
    expr: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let vars: Vec<(String, Value)> = parse_kv_pairs(expr)
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    with_scope(context, vars, body_mutates_context(inner), |c| {
        render_control_flow_internal(inner, c, preserve_unresolved)
    })
}

fn evaluate_firstof(expr: &str, context: &HashMap<String, Value>) -> String {
    for part in expr.split_whitespace() {
        let val = resolve_operand_cow(part, context);
        if is_truthy(&val) {
            return match &*val {
                Value::String(value) => {
                    let (payload, safe) = strip_safe_prefix(value);
                    if safe {
                        payload.to_string()
                    } else {
                        escape_html_text(payload)
                    }
                }
                other => escape_html_text(&format_value(other)),
            };
        }
    }
    String::new()
}

// Includes are cached by path + mtime: one `stat` per include instead of
// exists() x3 + read + server-block strip. Edits are still picked up instantly.
static INCLUDE_CACHE: LazyLock<RwLock<HashMap<PathBuf, (SystemTime, Arc<str>)>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn load_include(path: &Path) -> Option<Arc<str>> {
    let meta = std::fs::metadata(path).ok()?; // None → candidate doesn't exist
    let mtime = meta.modified().ok();

    if let Some(mt) = mtime {
        if let Ok(cache) = INCLUDE_CACHE.read() {
            if let Some((t, content)) = cache.get(path) {
                if *t == mt {
                    return Some(Arc::clone(content));
                }
            }
        }
    }

    let mut content = std::fs::read_to_string(path).unwrap_or_default();
    if let Some(start) = content.find("<script server>") {
        if let Some(end) = content[start..].find("</script>") {
            content.replace_range(start..start + end + 9, "");
        }
    }
    let content: Arc<str> = Arc::from(content);
    if let Some(mt) = mtime {
        if let Ok(mut cache) = INCLUDE_CACHE.write() {
            cache.insert(path.to_path_buf(), (mt, Arc::clone(&content)));
        }
    }
    Some(content)
}

fn evaluate_include(expr: &str, context: &mut HashMap<String, Value>, preserve_unresolved: bool) -> String {
    let mut parts = expr.split_whitespace();
    let Some(path_expr) = parts.next() else { return String::new(); };
    let path = path_expr.trim_matches('"').trim_matches('\'');

    let root = crate::state::get_project_root();
    let candidates = [
        root.join(format!("{}.vlo", path)),
        root.join("components").join(format!("{}.vlo", path)),
        root.join("partials").join(format!("{}.vlo", path)),
    ];
    let Some(content) = candidates.iter().find_map(|p| load_include(p)) else {
        return String::new();
    };

    let vars: Vec<(String, Value)> = match expr.find(" with ") {
        Some(with_idx) => parse_kv_pairs(&expr[with_idx + 6..])
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
        None => Vec::new(),
    };
    with_scope(context, vars, body_mutates_context(&content), |c| {
        render_control_flow_internal(&content, c, preserve_unresolved)
    })
}

fn render_else(
    else_part: Option<&str>,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let Some(v) = else_part else {
        return String::new();
    };
    let body = v.strip_prefix("{else}").unwrap_or(v);
    // The old code always rendered the else branch in a cloned scope.
    with_scope(context, Vec::new(), body_mutates_context(body), |c| {
        render_control_flow_internal(body, c, preserve_unresolved)
    })
}

// ═════════════════════════════════════════════════════════════
// Runtime query preservation
// ═════════════════════════════════════════════════════════════

/// Identifier-like words of an expression (same split as the old char loop).
#[inline]
fn expr_words(expression: &str) -> impl Iterator<Item = &str> {
    expression
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

fn expression_uses_runtime_query(expression: &str) -> bool {
    expr_words(expression).any(is_runtime_var)
}

fn all_vars_resolved(expression: &str, context: &HashMap<String, Value>) -> bool {
    expr_words(expression).all(|w| !is_runtime_var(w) || context.contains_key(w))
}

// [B6] Exact tag matching instead of substring.
fn block_uses_runtime_query(block: &str) -> bool {
    if expression_uses_runtime_query(block) {
        return true;
    }
    let mut cursor = 0usize;
    while cursor < block.len() {
        let Some((pos, token)) = find_next_token(block, cursor) else {
            break;
        };

        if token == "{{" {
            if let Some(end_rel) = block[pos + 2..].find("}}") {
                let end = pos + 2 + end_rel;
                if expression_uses_runtime_query(block[pos + 2..end].trim()) {
                    return true;
                }
                cursor = end + 2;
                continue;
            }
            break;
        }

        let tag = tag_name_of(token);
        if matches!(tag, "if" | "unless" | "switch") {
            let header_end = match find_unquoted_char(&block[pos..], '}') {
                Some(off) => pos + off,
                None => break,
            };
            let expr_start = pos + token.len();
            if expr_start <= header_end {
                let expression = block[expr_start..header_end].trim();
                if expression_uses_runtime_query(expression) {
                    return true;
                }
            }
            cursor = header_end + 1;
            continue;
        }

        cursor = pos + token.len();
    }
    false
}

/// One-pass marker restore (old code: one full `replace` pass per marker).
fn restore_markers(template: &str, prefix: &str, values: &[String]) -> String {
    if values.is_empty() || !template.contains(prefix) {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut cursor = 0usize;
    while let Some(rel) = template[cursor..].find(prefix) {
        let start = cursor + rel;
        out.push_str(&template[cursor..start]);
        let digits_start = start + prefix.len();
        let digits_len = template[digits_start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let digits_end = digits_start + digits_len;
        let replacement = (digits_len > 0 && template[digits_end..].starts_with("__"))
            .then(|| template[digits_start..digits_end].parse::<usize>().ok())
            .flatten()
            .and_then(|i| values.get(i));
        match replacement {
            Some(v) => {
                out.push_str(v);
                cursor = digits_end + 2;
            }
            None => {
                out.push_str(prefix);
                cursor = digits_start;
            }
        }
    }
    out.push_str(&template[cursor..]);
    out
}

pub fn preserve_runtime_query_interpolations(template: &str) -> (String, Vec<String>) {
    let mut result = String::with_capacity(template.len());
    let mut values = Vec::new();
    let mut cursor = 0usize;
    while cursor < template.len() {
        let Some(start_rel) = template[cursor..].find("{{") else {
            result.push_str(&template[cursor..]);
            break;
        };
        let start = cursor + start_rel;
        result.push_str(&template[cursor..start]);
        let Some(end_rel) = template[start + 2..].find("}}") else {
            result.push_str(&template[start..]);
            break;
        };
        let end = start + 2 + end_rel;
        let expression = template[start + 2..end].trim();
        if is_runtime_var(expression) {
            let index = values.len();
            values.push(template[start..end + 2].to_string());
            let _ = write!(result, "__VLO_RUNTIME_QUERY_INTERPOLATION_{}__", index);
        } else {
            result.push_str(&template[start..end + 2]);
        }
        cursor = end + 2;
    }
    (result, values)
}

pub fn restore_runtime_query_interpolations(template: &str, values: &[String]) -> String {
    restore_markers(template, "__VLO_RUNTIME_QUERY_INTERPOLATION_", values)
}

pub fn preserve_runtime_query_blocks(template: &str) -> (String, Vec<String>) {
    let mut result = String::with_capacity(template.len());
    let mut blocks = Vec::new();
    let mut cursor = 0usize;

    while cursor < template.len() {
        let Some((pos, token)) = find_next_token(template, cursor) else {
            result.push_str(&template[cursor..]);
            break;
        };
        if pos > cursor {
            result.push_str(&template[cursor..pos]);
        }

        // Skip non-block tokens — just copy them through.
        let tag = tag_name_of(token);
        if !matches!(tag, "if" | "unless" | "switch") {
            let advance = match token {
                "{{" => template[pos + 2..]
                    .find("}}")
                    .map(|p| pos + 2 + p + 2)
                    .unwrap_or(template.len()),
                "<script" | "</script>" | "<pre>" | "<pre " | "<code>" | "<code " | "<textarea>" | "<textarea " => {
                    // Advance past the whole opening tag; the walker will
                    // copy the rest verbatim in the next iteration.
                    template[pos..]
                        .find('>')
                        .map(|p| pos + p + 1)
                        .unwrap_or(template.len())
                }
                _ => pos + token.len(),
            };
            result.push_str(&template[pos..advance]);
            cursor = advance;
            continue;
        }

        let header_end = match find_unquoted_char(&template[pos..], '}') {
            Some(off) => pos + off,
            None => {
                result.push_str(&template[pos..]);
                break;
            }
        };

        let expr_start = pos + token.len();
        let expression = if expr_start <= header_end {
            template[expr_start..header_end].trim()
        } else {
            ""
        };

        let Some((_, block_end)) = find_block_end(template, pos, token) else {
            result.push_str(&template[pos..]);
            break;
        };
        let block = &template[pos..block_end];

        if expression_uses_runtime_query(expression) || block_uses_runtime_query(block) {
            let index = blocks.len();
            blocks.push(block.to_string());
            let _ = write!(result, "__VLO_RUNTIME_QUERY_BLOCK_{}__", index);
            cursor = block_end;
        } else {
            result.push_str(&template[pos..header_end + 1]);
            cursor = header_end + 1;
        }
    }
    (result, blocks)
}

pub fn restore_runtime_query_blocks(template: &str, blocks: &[String]) -> String {
    restore_markers(template, "__VLO_RUNTIME_QUERY_BLOCK_", blocks)
}

pub fn preserve_runtime_data_sources(template: &str) -> (String, Vec<String>) {
    let mut result = String::with_capacity(template.len());
    let mut blocks = Vec::new();
    let mut last_end = 0usize;
    let bytes = template.as_bytes();

    for cap in RUNTIME_DATA_SOURCE_RE.captures_iter(template) {
        let full = cap.get(0).unwrap();
        if full.start() < last_end {
            continue;
        }
        let tag = cap.get(1).unwrap().as_str();
        let (tb, tl) = (tag.as_bytes(), tag.len());

        // Single pass over '<' bytes: `</tag>` closes, `<tag[ >/]` opens.
        let mut depth = 1usize;
        let mut cursor = full.end();
        let mut close_start = None;

        while cursor < bytes.len() {
            let Some(p) = bytes[cursor..].iter().position(|&b| b == b'<') else {
                break;
            };
            let pos = cursor + p;
            let after = &bytes[pos + 1..];

            if after.first() == Some(&b'/')
                && after[1..].starts_with(tb)
                && after.get(1 + tl) == Some(&b'>')
            {
                depth -= 1;
                if depth == 0 {
                    close_start = Some(pos);
                    break;
                }
                cursor = pos + tl + 3;
                continue;
            }
            if after.starts_with(tb) {
                if matches!(after.get(tl), Some(b'>') | Some(b' ') | Some(b'/')) {
                    depth += 1;
                }
                cursor = pos + 1 + tl;
                continue;
            }
            cursor = pos + 1;
        }
        let Some(close_pos) = close_start else {
            continue;
        };

        let close_len = tl + 3;
        result.push_str(&template[last_end..full.end()]);
        let inner = &template[full.end()..close_pos];
        let index = blocks.len();
        blocks.push(inner.to_string());
        let _ = write!(result, "__VLO_RUNTIME_DATA_SOURCE_{}__</{}>", index, tag);
        last_end = close_pos + close_len;
    }
    result.push_str(&template[last_end..]);
    (result, blocks)
}

pub fn restore_runtime_data_sources(template: &str, blocks: &[String]) -> String {
    restore_markers(template, "__VLO_RUNTIME_DATA_SOURCE_", blocks)
}

// ═════════════════════════════════════════════════════════════
// Public entry points
// ═════════════════════════════════════════════════════════════
pub fn render_control_flow(template: &str, context: &HashMap<String, Value>) -> String {
    let preserve = crate::state::is_building();
    let mut local_ctx = context.clone();
    let out = render_control_flow_internal(template, &mut local_ctx, preserve);
    strip_flow_markers(out)
}

/// Renders `template` directly in `context` (no up-front deep clone).
/// Used by the component renderer, which overlays props on the page context
/// and restores it afterwards. If the template can write to the context
/// (`{assign}` / `{capture}`) it falls back to a private clone, so nothing leaks.
pub fn render_control_flow_in_scope(
    template: &str,
    context: &mut HashMap<String, Value>,
) -> String {
    let preserve = crate::state::is_building();
    if body_mutates_context(template) {
        let mut local = context.clone();
        return strip_flow_markers(render_control_flow_internal(template, &mut local, preserve));
    }
    strip_flow_markers(render_control_flow_internal(template, context, preserve))
}

pub fn render_control_flow_for_build(
    template: &str,
    context: &HashMap<String, Value>,
) -> String {
    let (protected, blocks) = preserve_runtime_query_blocks(template);

    // Build the protected text in one pass (old: replace_range in a loop = O(n·k)).
    let mut protected_interpolations: Vec<String> = Vec::new();
    let mut source = String::with_capacity(protected.len());
    let mut cursor = 0usize;

    while let Some(start_rel) = protected[cursor..].find("{{") {
        let start = cursor + start_rel;
        let Some(end_rel) = protected[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + end_rel;
        let expression = protected[start + 2..end].trim();

        // Only protect the base variable, not modifier arguments
        let base_key = expression.split('|').next().unwrap_or(expression).trim();
        source.push_str(&protected[cursor..start]);
        if base_key.contains('.') || is_runtime_var(base_key) {
            let index = protected_interpolations.len();
            protected_interpolations.push(protected[start..end + 2].to_string());
            let _ = write!(source, "__VLO_RUNTIME_INTERPOLATION_{}__", index);
        } else {
            source.push_str(&protected[start..end + 2]);
        }
        cursor = end + 2;
    }
    source.push_str(&protected[cursor..]);

    let mut local_ctx = context.clone();
    let rendered = render_control_flow_internal(&source, &mut local_ctx, true);

    let rendered = restore_markers(&rendered, "__VLO_RUNTIME_INTERPOLATION_", &protected_interpolations);
    let restored = restore_runtime_query_blocks(&rendered, &blocks);
    strip_flow_markers(restored)
}

pub fn render_interpolations(template: &str, context: &HashMap<String, Value>) -> String {
    render_interpolations_mode(template, context, true)
}

/// Interpolation for non-HTML sinks (URLs, email subjects): no HTML escaping.
pub fn render_interpolations_raw(template: &str, context: &HashMap<String, Value>) -> String {
    render_interpolations_mode(template, context, false)
}

fn render_interpolations_mode(
    template: &str,
    context: &HashMap<String, Value>,
    html_escape: bool,
) -> String {
    if !template.contains("{{") {
        return template.to_string();
    }
    let preserve = crate::state::is_building();
    let in_quotes: OnceCell<Vec<bool>> = OnceCell::new();

    PROP_RE
        .replace_all(template, |captures: &regex::Captures| {
            let full = captures.get(0).unwrap();
            let key = captures[1].trim();
            let base_key = key.split('|').next().unwrap_or(key).trim();

            if preserve && !is_literal_operand(base_key) && !context.contains_key(base_key) {
                return full.as_str().to_string();
            }

            let quoted =
                html_escape && in_quotes.get_or_init(|| quote_mask(template))[full.start()];
            let mut out = String::new();
            if let Some((base, chain)) = key.split_once('|') {
                let value = resolve_operand_stripped(base.trim(), context);
                let value = crate::modifier::apply(value, chain);
                let safe_output =
                    !html_escape || crate::modifier::chain_is_html_output(chain);
                push_rendered_interpolation(&mut out, &value, quoted, safe_output);
            } else {
                let value = resolve_operand_cow(key, context);
                push_value_interpolation(&mut out, &value, quoted, !html_escape);
            }
            out
        })
        .into_owned()
}

/// Emits a value that has already been through modifiers.
#[inline]
fn push_rendered_interpolation(out: &mut String, value: &str, quoted: bool, safe_output: bool) {
    let value = strip_safe_markers(value);
    if quoted {
        push_attr_escaped(out, &value);
    } else if safe_output {
        out.push_str(&value);
    } else {
        push_text_escaped(out, &value);
    }
}

#[inline]
fn push_value_interpolation(
    out: &mut String,
    value: &Value,
    quoted: bool,
    raw_output: bool,
) {
    if let Value::String(value) = value {
        let (payload, safe) = strip_safe_prefix(value);
        let payload = strip_safe_markers(payload);
        if quoted {
            push_attr_escaped(out, &payload);
        } else if safe || raw_output {
            out.push_str(&payload);
        } else {
            push_text_escaped(out, &payload);
        }
    } else {
        let mut tmp = String::new();
        append_value(&mut tmp, value);
        if quoted {
            push_attr_escaped(out, &tmp);
        } else if raw_output {
            out.push_str(&tmp);
        } else {
            push_text_escaped(out, &tmp);
        }
    }
}

/// Appends `format_value(val)` without the intermediate String.
#[inline]
fn append_value(out: &mut String, val: &Value) {
    match val {
        Value::Null => {}
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let _ = write!(out, "{}", n);
        }
        Value::String(s) => out.push_str(s),
        other => {
            if let Ok(s) = serde_json::to_string(other) {
                out.push_str(&s);
            }
        }
    }
}

pub fn format_value(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        other => {
            let mut s = String::new();
            append_value(&mut s, other);
            s
        }
    }
}

pub fn clean_empty_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    for line in html.lines() {
        if !line.trim().is_empty() {
            result.push_str(line);
            result.push('\n');
        }
    }
    if !result.is_empty() {
        result.pop();
    }
    result
}

/// Single-pass attribute escaping (old: five chained `replace` allocations).
#[inline]
fn push_attr_escaped(out: &mut String, value: &str) {
    out.reserve(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

/// Text-context escaping. Braces are encoded too so user data containing
/// `{{...}}` cannot be re-interpreted by a later interpolation pass.
#[inline]
fn push_text_escaped(out: &mut String, value: &str) {
    out.reserve(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '{' => out.push_str("&#123;"),
            '}' => out.push_str("&#125;"),
            c => out.push(c),
        }
    }
}

pub fn escape_html_text(value: &str) -> String {
    let mut out = String::new();
    push_text_escaped(&mut out, value);
    out
}

pub fn escape_html_attribute(value: &str) -> String {
    let mut out = String::new();
    push_attr_escaped(&mut out, value);
    out
}

// ═════════════════════════════════════════════════════════════
// Core walker
// ═════════════════════════════════════════════════════════════
fn render_control_flow_internal(
    template: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    render_walk(template, None, context, preserve_unresolved)
}

#[inline]
fn raw_close_tag(token: &str) -> &'static str {
    match token {
        "<script" => "</script>",
        "<pre>" | "<pre " => "</pre>",
        "<code>" | "<code " => "</code>",
        _ => "</textarea>",
    }
}

/// `pre_mask` lets a loop compute the body's quote mask once; otherwise the
/// mask is built lazily — only if the template actually contains a `{{`.
fn render_walk(
    template: &str,
    pre_mask: Option<&[bool]>,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let mut result = String::with_capacity(template.len());
    let mut cursor = 0usize;
    let lazy_mask: OnceCell<Vec<bool>> = OnceCell::new();

    while cursor < template.len() {
        let Some((pos, token)) = find_next_token(template, cursor) else {
            result.push_str(&template[cursor..]);
            break;
        };
        if pos > cursor {
            result.push_str(&template[cursor..pos]);
        }

        if matches!(token, "<script" | "<pre>" | "<pre " | "<code>" | "<code " | "<textarea>" | "<textarea ") {
            let close_tag = raw_close_tag(token);
            let search_start = template[pos..].find('>').map(|i| pos + i + 1).unwrap_or(pos);
            if let Some(end_rel) = template[search_start..].find(close_tag) {
                let end = search_start + end_rel + close_tag.len();
                result.push_str(&template[pos..end]);
                cursor = end;
                continue;
            }
            result.push_str(&template[pos..]);
            break;
        }

        if token == "{{" {
            if let Some(end_rel) = template[pos + 2..].find("}}") {
                let end = pos + 2 + end_rel;
                let key = template[pos + 2..end].trim();
                let (base_key, chain) = match key.split_once('|') {
                    Some((b, c)) => (b.trim(), Some(c)),
                    None => (key, None),
                };

                if preserve_unresolved
                    && !is_literal_operand(base_key)
                    && !context.contains_key(base_key)
                {
                    result.push_str(&template[pos..end + 2]);
                    cursor = end + 2;
                    continue;
                }

                let mask: &[bool] = match pre_mask {
                    Some(m) => m,
                    None => lazy_mask.get_or_init(|| quote_mask(template)),
                };
                let quoted = mask[pos];

                match chain {
                    Some(chain) => {
                        let value = resolve_operand_stripped(base_key, context);
                        let value = crate::modifier::apply(value, chain);
                        let safe_output = crate::modifier::chain_is_html_output(chain);
                        push_rendered_interpolation(&mut result, &value, quoted, safe_output);
                    }
                    None => {
                        let value = resolve_operand_cow(key, context);
                        push_value_interpolation(&mut result, &value, quoted, false);
                    }
                }
                cursor = end + 2;
            } else {
                result.push_str(&template[pos..]);
                break;
            }
            continue;
        }

        let tag = tag_name_of(token);

        let header_end = match find_unquoted_char(&template[pos..], '}') {
            Some(off) => pos + off + 1,
            None => { result.push_str(&template[pos..]); break; }
        };

        let expr_start = pos + token.len();
        let expr_end = header_end - 1;
        let expression = if expr_start <= expr_end { template[expr_start..expr_end].trim() } else { "" };

        match tag {
            "break" => { result.push_str(BREAK_MARKER); cursor = header_end; continue; }
            "continue" => { result.push_str(CONTINUE_MARKER); cursor = header_end; continue; }
            "firstof" => { result.push_str(&evaluate_firstof(expression, context)); cursor = header_end; continue; }
            "assign" => { evaluate_assign(expression, context); cursor = header_end; continue; }
            "include" => { result.push_str(&evaluate_include(expression, context, preserve_unresolved)); cursor = header_end; continue; }
            "raw" | "comment" => {
                let close_tag = if tag == "raw" { "{/raw}" } else { "{/comment}" };
                if let Some(end_rel) = template[header_end..].find(close_tag) {
                    let end = header_end + end_rel + close_tag.len();
                    if tag == "raw" { result.push_str(&template[header_end..header_end + end_rel]); }
                    cursor = end;
                } else {
                    result.push_str(&template[pos..header_end]);
                    cursor = header_end;
                }
                continue;
            }
            _ => {}
        }

        let Some((content_start, block_end)) = find_block_end(template, pos, token) else {
            result.push_str(&template[pos..]);
            break;
        };

        let closing_len = tag.len() + 3;
        let inner_end = block_end.saturating_sub(closing_len);
        let inner = if content_start <= inner_end && inner_end <= template.len() {
            &template[content_start..inner_end]
        } else {
            result.push_str(&template[pos..block_end]);
            cursor = block_end;
            continue;
        };

        match tag {
            "if" => result.push_str(&evaluate_if_block(inner, expression, context, preserve_unresolved)),
            "for" => result.push_str(&for_block_scoped(inner, expression, context, preserve_unresolved)),
            "unless" => result.push_str(&evaluate_unless_block(inner, expression, context, preserve_unresolved)),
            "switch" => result.push_str(&evaluate_switch_block(inner, expression, context, preserve_unresolved)),
            "capture" => evaluate_capture(inner, expression, context, preserve_unresolved),
            "with" => result.push_str(&evaluate_with(inner, expression, context, preserve_unresolved)),
            _ => {}
        }
        cursor = block_end;
    }

    result
}

fn strip_flow_markers(s: String) -> String {
    if !s.as_bytes().contains(&1) {
        return s;
    }
    s.replace(BREAK_MARKER, "")
        .replace(CONTINUE_MARKER, "")
        .replace(SAFE_HTML_MARKER, "")
}

fn is_runtime_var(var: &str) -> bool {
    matches!(
        var,
        "status" | "action" | "message" |
        "search" | "sort" | "order" | "_columns" |
        "page" | "limit" | "offset" | "total" | "total_pages" |
        "has_next" | "has_prev" | "prev_page" | "next_page" |
        "logged_in" | "user_name" | "user_role" | "user_email" |
        "flash_messages" | "flash_variant" | "flash_icon" | "flash_title" | "flash_description" |
        "variant" | "icon" | "title" | "description" | "duration" |
        "csrf_token" | "auth_identifier_field" | "auth_password_field" |
        "data" | "meta" | "config"  |
        "tab" | "step" | "inv"
    )
}