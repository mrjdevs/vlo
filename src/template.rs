use crate::state::PROP_RE;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::LazyLock,
};

static RUNTIME_DATA_SOURCE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"<([a-zA-Z][a-zA-Z0-9-]*)\s+([^>]*?)data-source\s*=\s*["']([^"']+)["']([^>]*?)>"#,
    )
    .unwrap()
});

// ______________________ Quote mask ____________________

fn quote_mask(template: &str) -> Vec<bool> {
    let len = template.len();
    let mut mask = vec![false; len + 1];

    // State machine:
    //   in_raw  - inside a <script> or <style> block (skip until closing tag)
    //   in_tag  - between `<` and `>` in normal content
    //   quote   - if Some, inside an attribute quote
    let mut in_raw = false;
    let mut raw_close: &'static str = "";

    let mut in_tag = false;
    let mut quote: Option<char> = None;

    let mut i = 0usize;
    while i < len {
        // ── Inside <script>...</script> or <style>...</style> ──
        if in_raw {
            if template[i..].starts_with(raw_close) {
                in_raw = false;
                i += raw_close.len();
                continue;
            }
            // Advance by full UTF-8 char length, not one byte — the raw
            i += template[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            continue;
        }

        // ── Normal content ──
        let ch = template[i..].chars().next().unwrap();
        mask[i] = in_tag && quote.is_some();

        if in_tag {
            match quote {
                Some(active) => {
                    if ch == active {
                        quote = None;
                    }
                }
                None => match ch {
                    '"' | '\'' => quote = Some(ch),
                    '>' => in_tag = false,
                    _ => {}
                },
            }
        } else if ch == '<' {
            // Detect entry into a raw-text region before treating
            // this as a regular tag.
            if template[i..].starts_with("<script") {
                in_raw = true;
                raw_close = "</script>";
            } else if template[i..].starts_with("<style") {
                in_raw = true;
                raw_close = "</style>";
            } else {
                in_tag = true;
            }
        }

        i += ch.len_utf8();
    }

    mask[len] = in_tag && quote.is_some();
    mask
}

/// [G4] Find `target` at quote-depth 0. Skips escaped quotes.

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Advance a byte cursor past one full UTF-8 character.
/// Returns the new cursor position, or `len` if at the end.
#[inline]
fn advance_char(s: &str, cursor: usize) -> usize {
    s[cursor..]
        .chars()
        .next()
        .map(|c| cursor + c.len_utf8())
        .unwrap_or(s.len())
}

fn find_unquoted_char(s: &str, target: char) -> Option<usize> {
    let mut in_quotes: Option<char> = None;
    let bytes = s.as_bytes();

    for (i, ch) in s.char_indices() {
        if let Some(q) = in_quotes {
            if ch == q && (i == 0 || bytes[i - 1] != b'\\') {
                in_quotes = None;
            }
        } else if ch == '"' || ch == '\'' {
            in_quotes = Some(ch);
        } else if ch == target {
            return Some(i);
        }
    }
    None
}

pub fn get_nested_value(path: &str, context: &HashMap<String, Value>) -> Value {
    let path = path.trim();
    if path.is_empty() {
        return Value::Null;
    }

    let mut parts = path.split('.');
    let first = parts.next().unwrap();
    let mut current = context.get(first);

    for part in parts {
        match current {
            Some(Value::Object(map)) => current = map.get(part),
            Some(Value::Array(arr)) => {
                if let Ok(idx) = part.parse::<usize>() {
                    current = arr.get(idx);
                } else {
                    return Value::Null;
                }
            }
            _ => return Value::Null,
        }
    }

    current.cloned().unwrap_or(Value::Null)
}

// ═════════════════════════════════════════════════════════════
// Expression language
// ═════════════════════════════════════════════════════════════
pub fn evaluate_condition(expr: &str, context: &HashMap<String, Value>) -> bool {
    let expr = expr.trim();
    if expr.is_empty() {
        return false;
    }
    evaluate_or(expr, context)
}

fn evaluate_or(expr: &str, context: &HashMap<String, Value>) -> bool {
    for branch in split_logical(expr, "||") {
        if evaluate_and(branch.trim(), context) {
            return true;
        }
    }
    false
}

fn evaluate_and(expr: &str, context: &HashMap<String, Value>) -> bool {
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

    // Word operators.
    for op in ["not in", "in", "contains"] {
        if let Some(pos) = find_word_operator(expr, op) {
            let left = resolve_operand(&expr[..pos], context);
            let right = resolve_operand(&expr[pos + op.len()..], context);
            return match op {
                "in" => value_in(&left, &right),
                "not in" => !value_in(&left, &right),
                "contains" => value_in(&right, &left),
                _ => false,
            };
        }
    }

    for op in ["==", "!=", "<=", ">=", "<", ">"] {
        if let Some(pos) = find_operator(expr, op) {
            let left = resolve_operand(&expr[..pos], context);
            let right = resolve_operand(&expr[pos + op.len()..], context);
            return compare_values(&left, &right, op);
        }
    }

    is_truthy(&resolve_operand(expr, context))
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

fn resolve_operand(operand: &str, context: &HashMap<String, Value>) -> Value {
    let trimmed = operand.trim();

    if trimmed == "true" {
        return Value::Bool(true);
    }
    if trimmed == "false" {
        return Value::Bool(false);
    }
    if trimmed == "null" || trimmed == "nil" {
        return Value::Null;
    }

    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return Value::String(trimmed[1..trimmed.len() - 1].to_string());
    }
    if let Ok(num) = trimmed.parse::<i64>() {
        return Value::Number(num.into());
    }
    if let Ok(num) = trimmed.parse::<f64>() {
        return serde_json::Number::from_f64(num)
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    get_nested_value(trimmed, context)
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
// ═════════════════════════════════════════════════════════════
pub fn find_next_token(template: &str, from: usize) -> Option<(usize, &'static str)> {
    let mut best: Option<(usize, &'static str)> = None;

    // [B2] Real element openers only. `<pre` matches `<presto>` otherwise.
    const TOKENS: &[&str] = &[
        // Blocks.
        "{if ",
        "{for ",
        "{unless ",
        "{switch ",
        "{with ",
        "{capture ",
        "{capture}",
        "{assign ",
        "{firstof ",
        "{include ",
        "{raw}",
        "{comment}",
        "{break}",
        "{continue}",
        "{else if ",
        "{else}",
        "{/if}",
        "{/for}",
        "{/unless}",
        "{/switch}",
        "{/with}",
        "{/capture}",
        "{/raw}",
        "{/comment}",
        "{case ",
        "{default}",
        // Interpolation.
        "{{",
        // Script boundary.
        "<script",
        "</script>",
        // [B2] Literal-content regions — real openers only.
        "<pre>",
        "<pre ",
        "<code>",
        "<code ",
        "<textarea>",
        "<textarea ",
    ];

    for token in TOKENS {
        if let Some(pos) = template[from..].find(token) {
            let absolute = from + pos;
            if best.map(|(p, _)| absolute < p).unwrap_or(true) {
                best = Some((absolute, token));
            }
        }
    }
    best
}

// ═════════════════════════════════════════════════════════════
// Block end finder
// ═════════════════════════════════════════════════════════════
pub fn find_block_end(template: &str, start: usize, token: &str) -> Option<(usize, usize)> {
    let kind = token
        .trim_start_matches('{')
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches('}');

    if kind.is_empty() {
        return None;
    }

    // [G2] Quote-aware header end.
    let rest = &template[start..];
    let header_offset = find_unquoted_char(rest, '}')?;
    let header_end = start + header_offset + 1;

    // [B3] Only the space-form counts as a nested opener.
    let open_pattern = format!("{{{kind} ");
    let open_pattern_bare = format!("{{{kind}}}");
    let close_pattern = format!("{{/{kind}}}");

    let mut depth = 1usize;
    let mut cursor = header_end;

    while cursor < template.len() {
        let next_open_a = template[cursor..].find(&open_pattern).map(|p| cursor + p);
        let next_open_b = template[cursor..].find(&open_pattern_bare).map(|p| cursor + p);
        let next_open = match (next_open_a, next_open_b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let next_close = template[cursor..]
            .find(&close_pattern)
            .map(|p| cursor + p);

        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                cursor = o + open_pattern.len().max(open_pattern_bare.len());
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    return Some((header_end, c + close_pattern.len()));
                }
                cursor = c + close_pattern.len();
            }
            _ => return None,
        }
    }
    None
}

// ═════════════════════════════════════════════════════════════
// Branch splitting — single-brace only [G1]
// ═════════════════════════════════════════════════════════════
pub fn split_else_branches(inner: &str) -> (String, Option<String>) {
    let mut depth = 0usize;
    let mut cursor = 0usize;

    let targets = [
        ("{if ", 0),
        ("{for ", 0),
        ("{unless ", 0),
        ("{switch ", 0),
        ("{with ", 0),
        ("{capture ", 0),
        ("{capture}", 0),
        ("{/if}", 1),
        ("{/for}", 1),
        ("{/unless}", 1),
        ("{/switch}", 1),
        ("{/with}", 1),
        ("{/capture}", 1),
        ("{else if ", 2),
        ("{else}", 3),
    ];

    while cursor < inner.len() {
        let mut candidate: Option<(usize, u8, usize)> = None;
        for (pat, kind) in targets {
            if let Some(p) = inner[cursor..].find(pat) {
                let abs = cursor + p;
                if candidate.map(|(cp, _, _)| abs < cp).unwrap_or(true) {
                    candidate = Some((abs, kind, pat.len()));
                }
            }
        }
        let Some((pos, kind, len)) = candidate else {
            break;
        };
        match kind {
            0 => {
                depth += 1;
                cursor = pos + len;
            }
            1 => {
                if depth > 0 {
                    depth -= 1;
                }
                cursor = pos + len;
            }
            2 | 3 if depth == 0 => {
                return (inner[..pos].to_string(), Some(inner[pos..].to_string()));
            }
            _ => {
                cursor = pos + len;
            }
        }
    }
    (inner.to_string(), None)
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
    if preserve_unresolved && expression_uses_runtime_query(expression) {
        if !all_vars_resolved(expression, context) {
            return format!("{{if {}}}{}{{/if}}", expression, inner);
        }
    }

    let (true_part, else_part) = split_else_branches(inner);

    if evaluate_condition(expression, context) {
        return render_control_flow_internal(&true_part, context, preserve_unresolved);
    }

    if let Some(rest) = else_part {
        if let Some(v) = rest.strip_prefix("{else if ") {
            if let Some(end) = v.find('}') {
                let expr = v[..end].trim();
                let body = &v[end + 1..];
                return evaluate_if_block(body, expr, context, preserve_unresolved);
            }
        }
        let else_body = rest.strip_prefix("{else}").unwrap_or(&rest);
        return render_control_flow_internal(else_body, context, preserve_unresolved);
    }

    String::new()
}

pub fn evaluate_unless_block(
    inner: &str,
    expression: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let (true_part, else_part) = split_else_branches(inner);
    if !evaluate_condition(expression, context) {
        return render_control_flow_internal(&true_part, context, preserve_unresolved);
    }
    if let Some(rest) = else_part {
        if let Some(v) = rest.strip_prefix("{else if ") {
            if let Some(end) = v.find('}') {
                let expr = v[..end].trim();
                let body = &v[end + 1..];
                return evaluate_if_block(body, expr, context, preserve_unresolved);
            }
        }
        let else_body = rest.strip_prefix("{else}").unwrap_or(&rest);
        return render_control_flow_internal(else_body, context, preserve_unresolved);
    }
    String::new()
}
// ═════════════════════════════════════════════════════════════
// Switch — nested-switch aware [B4]
// ═════════════════════════════════════════════════════════════
fn find_next_case_or_default(inner: &str, from: usize) -> Option<(usize, bool)> {
    let mut cursor = from;
    let mut switch_depth = 0usize;

    while cursor < inner.len() {
        if inner[cursor..].starts_with("{switch ") {
            switch_depth += 1;
            cursor += 8;
            continue;
        }
        if inner[cursor..].starts_with("{/switch}") {
            if switch_depth == 0 {
                break;
            }
            switch_depth -= 1;
            cursor += 9;
            continue;
        }
        if switch_depth == 0 {
            if inner[cursor..].starts_with("{case ") {
                return Some((cursor, false));
            }
            if inner[cursor..].starts_with("{default") {
                return Some((cursor, true));
            }
        }
        cursor = advance_char(inner, cursor);
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
        let case_val = resolve_operand(case_val_expr, context);
        let body_start = header_close + 1;
        let body_end = find_next_case_or_default(inner, body_start)
            .map(|(p, _)| p)
            .unwrap_or(inner.len());

        // 🔥 FIX: exact match first, then case-insensitive fallback for strings
        let matched = compare_values(&subject, &case_val, "==")
            || match (&subject, &case_val) {
                (Value::String(a), Value::String(b)) => {
                    a.trim().eq_ignore_ascii_case(b.trim())
                }
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
// ═════════════════════════════════════════════════════════════
const BREAK_MARKER: &str = "\u{0001}VLO_BREAK\u{0001}";
const CONTINUE_MARKER: &str = "\u{0001}VLO_CONTINUE\u{0001}";

pub fn evaluate_for_block(
    inner: &str,
    expression: &str,
    context: &HashMap<String, Value>,
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

    let mut array_expr_parts = Vec::new();
    let mut limit: Option<usize> = None;
    let mut offset: Option<usize> = None;
    let mut reversed = false;

    for p in parts {
        if let Some(val) = p.strip_prefix("limit:") { limit = val.parse().ok(); }
        else if let Some(val) = p.strip_prefix("offset:") { offset = val.parse().ok(); }
        else if p == "reversed" { reversed = true; }
        else { array_expr_parts.push(p); }
    }

    let array_expr = array_expr_parts.join(" ");
    let array = get_nested_value(&array_expr, context);
    let (body, else_part) = split_else_branches(inner);

    let Value::Array(items) = array else {
        let mut fallback_ctx = context.clone();
        return render_else(else_part, &mut fallback_ctx, preserve_unresolved);
    };
    if items.is_empty() {
        let mut fallback_ctx = context.clone();
        return render_else(else_part, &mut fallback_ctx, preserve_unresolved);
    }

    let mut items_all: Vec<Value> = items;
    if reversed { items_all.reverse(); }

    let off = offset.unwrap_or(0);
    let lim = limit.unwrap_or(items_all.len());
    let items_to_iter: Vec<Value> = items_all.into_iter().skip(off).take(lim).collect();

    if items_to_iter.is_empty() {
        let mut fallback_ctx = context.clone();
        return render_else(else_part, &mut fallback_ctx, preserve_unresolved);
    }

    let count = items_to_iter.len();
    let mut result = String::with_capacity(body.len() * count);

    for (idx, item) in items_to_iter.into_iter().enumerate() {
        let mut child = context.clone();
        child.insert(item_var.to_string(), item);
        child.insert("@index".to_string(), Value::Number(((idx + off) as i64).into()));
        child.insert("@number".to_string(), Value::Number(((idx + off + 1) as i64).into()));
        child.insert("@first".to_string(), Value::Bool(idx == 0));
        child.insert("@last".to_string(), Value::Bool(idx + 1 == count));
        child.insert("@count".to_string(), Value::Number((count as i64).into()));
        child.insert("@remaining".to_string(), Value::Number(((count - idx - 1) as i64).into()));

        let rendered = render_control_flow_internal(&body, &mut child, preserve_unresolved);

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
        context.insert(trimmed.to_string(), Value::String(rendered));
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
    context: &HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let mut child = context.clone();
    for (k, v) in parse_kv_pairs(expr) {
        child.insert(k, Value::String(v));
    }
    render_control_flow_internal(inner, &mut child, preserve_unresolved)
}

fn evaluate_firstof(expr: &str, context: &HashMap<String, Value>) -> String {
    for part in expr.split_whitespace() {
        let val = resolve_operand(part, context);
        if is_truthy(&val) {
            return format_value(&val);
        }
    }
    String::new()
}

fn evaluate_include(expr: &str, context: &HashMap<String, Value>, preserve_unresolved: bool) -> String {
    let mut parts = expr.split_whitespace();
    let Some(path_expr) = parts.next() else { return String::new(); };
    let path = path_expr.trim_matches('"').trim_matches('\'');

    let root = crate::state::get_project_root();
    let candidates = [
        root.join(format!("{}.vlo", path)),
        root.join("components").join(format!("{}.vlo", path)),
        root.join("partials").join(format!("{}.vlo", path)),
    ];
    let Some(file_path) = candidates.into_iter().find(|p| p.exists()) else { return String::new(); };

    let mut content = match std::fs::read_to_string(&file_path) { Ok(c) => c, Err(_) => return String::new() };
    if let Some(start) = content.find("<script server>") {
        if let Some(end) = content[start..].find("</script>") { content.replace_range(start..start + end + 9, ""); }
    }

    let mut child = context.clone();
    if let Some(with_idx) = expr.find(" with ") {
        for (k, v) in parse_kv_pairs(&expr[with_idx + 6..]) { child.insert(k, Value::String(v)); }
    }
    render_control_flow_internal(&content, &mut child, preserve_unresolved)
}

fn render_else(
    else_part: Option<String>,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    else_part
        .map(|v| {
            render_control_flow_internal(
                v.strip_prefix("{else}").unwrap_or(&v),
                context,
                preserve_unresolved,
            )
        })
        .unwrap_or_default()
}

// ═════════════════════════════════════════════════════════════
// Runtime query preservation
// ═════════════════════════════════════════════════════════════
fn expression_uses_runtime_query(expression: &str) -> bool {
    let mut current = String::new();
    for ch in expression.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
            continue;
        }
        if !current.is_empty() {
            if is_runtime_var(&current) {
                return true;
            }
            current.clear();
        }
    }
    !current.is_empty() && is_runtime_var(&current)
}

fn all_vars_resolved(expression: &str, context: &HashMap<String, Value>) -> bool {
    let mut current = String::new();
    for ch in expression.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
        } else {
            if !current.is_empty() && is_runtime_var(&current) && !context.contains_key(&current) {
                return false;
            }
            current.clear();
        }
    }
    if !current.is_empty() && is_runtime_var(&current) && !context.contains_key(&current) {
        return false;
    }
    true
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
            result.push_str(&format!("__VLO_RUNTIME_QUERY_INTERPOLATION_{}__", index));
        } else {
            result.push_str(&template[start..end + 2]);
        }
        cursor = end + 2;
    }
    (result, values)
}

pub fn restore_runtime_query_interpolations(template: &str, values: &[String]) -> String {
    let mut result = template.to_string();
    for (index, value) in values.iter().enumerate() {
        result = result.replace(
            &format!("__VLO_RUNTIME_QUERY_INTERPOLATION_{}__", index),
            value,
        );
    }
    result
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
            result.push_str(&format!("__VLO_RUNTIME_QUERY_BLOCK_{}__", index));
            cursor = block_end;
        } else {
            result.push_str(&template[pos..header_end + 1]);
            cursor = header_end + 1;
        }
    }
    (result, blocks)
}

pub fn restore_runtime_query_blocks(template: &str, blocks: &[String]) -> String {
    let mut result = template.to_string();
    for (index, block) in blocks.iter().enumerate() {
        result = result.replace(&format!("__VLO_RUNTIME_QUERY_BLOCK_{}__", index), block);
    }
    result
}

pub fn preserve_runtime_data_sources(template: &str) -> (String, Vec<String>) {
    let mut result = String::with_capacity(template.len());
    let mut blocks = Vec::new();
    let mut last_end = 0usize;

    for cap in RUNTIME_DATA_SOURCE_RE.captures_iter(template) {
        let full = cap.get(0).unwrap();
        if full.start() < last_end {
            continue;
        }
        let tag = cap.get(1).unwrap().as_str();
        let close = format!("</{}>", tag);
        let open = format!("<{}", tag);
        let mut depth = 1usize;
        let mut cursor = full.end();
        let mut close_start = None;

        while cursor < template.len() {
            let next_open = template[cursor..].find(&open).map(|p| cursor + p);
            let next_close = template[cursor..].find(&close).map(|p| cursor + p);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    let after_open = o + open.len();
                    if template
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
            continue;
        };

        result.push_str(&template[last_end..full.end()]);
        let inner = &template[full.end()..close_pos];
        let index = blocks.len();
        blocks.push(inner.to_string());
        result.push_str(&format!("__VLO_RUNTIME_DATA_SOURCE_{}__", index));
        result.push_str(&close);
        last_end = close_pos + close.len();
    }
    result.push_str(&template[last_end..]);
    (result, blocks)
}

pub fn restore_runtime_data_sources(template: &str, blocks: &[String]) -> String {
    let mut result = template.to_string();
    for (index, block) in blocks.iter().enumerate() {
        result = result.replace(&format!("__VLO_RUNTIME_DATA_SOURCE_{}__", index), block);
    }
    result
}

// ═════════════════════════════════════════════════════════════
// Public entry points
// ═════════════════════════════════════════════════════════════
pub fn render_control_flow(template: &str, context: &HashMap<String, Value>) -> String {
    let preserve = crate::state::is_building();
    let mut local_ctx = context.clone();
    let out = render_control_flow_internal(template, &mut local_ctx, preserve);
    strip_flow_markers(&out)
}

pub fn render_control_flow_for_build(
    template: &str,
    context: &HashMap<String, Value>,
) -> String {
    let (protected, blocks) = preserve_runtime_query_blocks(template);

    let mut protected_interpolations = Vec::new();
    let mut source = protected;
    let mut cursor = 0usize;

    while let Some(start_rel) = source[cursor..].find("{{") {
        let start = cursor + start_rel;
        let Some(end_rel) = source[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + end_rel;
        let expression = source[start + 2..end].trim();
        if expression.contains('.') || is_runtime_var(expression) {
            let index = protected_interpolations.len();
            protected_interpolations.push(source[start..end + 2].to_string());
            let marker = format!("__VLO_RUNTIME_INTERPOLATION_{}__", index);
            source.replace_range(start..end + 2, &marker);
            cursor = start + marker.len();
        } else {
            cursor = end + 2;
        }
    }

    let mut local_ctx = context.clone();
    let mut rendered = render_control_flow_internal(&source, &mut local_ctx, true);

    for (index, interpolation) in protected_interpolations.iter().enumerate() {
        let marker = format!("__VLO_RUNTIME_INTERPOLATION_{}__", index);
        if rendered.contains(&marker) {
            rendered = rendered.replace(&marker, interpolation);
        }
    }
    let restored = restore_runtime_query_blocks(&rendered, &blocks);
    strip_flow_markers(&restored)
}

pub fn render_interpolations(template: &str, context: &HashMap<String, Value>) -> String {
    if !template.contains("{{") {
        return template.to_string();
    }
    let preserve = crate::state::is_building();
    let in_quotes = quote_mask(template);

    PROP_RE
        .replace_all(template, |captures: &regex::Captures| {
            let full = captures.get(0).unwrap();
            let key = captures[1].trim();
            let base_key = key.split('|').next().unwrap_or(key).trim();

            if preserve && !is_literal_operand(base_key) && !context.contains_key(base_key) {
                return full.as_str().to_string();
            }

            let value = if let Some((base, chain)) = key.split_once('|') {
                crate::modifier::apply(resolve_operand(base.trim(), context), chain)
            } else {
                format_value(&resolve_operand(key, context))
            };

            if in_quotes[full.start()] {
                escape_html_attribute(&value)
            } else {
                value
            }
        })
        .into_owned()
}

pub fn format_value(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => serde_json::to_string(val).unwrap_or_default(),
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

pub fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ═════════════════════════════════════════════════════════════
// Core walker
// ═════════════════════════════════════════════════════════════
fn render_control_flow_internal(
    template: &str,
    context: &mut HashMap<String, Value>,
    preserve_unresolved: bool,
) -> String {
    let mut result = String::with_capacity(template.len());
    let mut cursor = 0usize;
    let in_quotes = quote_mask(template);

    while cursor < template.len() {
        let Some((pos, token)) = find_next_token(template, cursor) else {
            result.push_str(&template[cursor..]);
            break;
        };
        if pos > cursor {
            result.push_str(&template[cursor..pos]);
        }

        if matches!(token, "<script" | "<pre>" | "<pre " | "<code>" | "<code " | "<textarea>" | "<textarea ") {
            let tag_name = token.trim_start_matches('<').trim_end_matches(|c: char| !c.is_ascii_alphabetic());
            let close_tag = format!("</{}>", tag_name);
            let search_start = template[pos..].find('>').map(|i| pos + i + 1).unwrap_or(pos);
            if let Some(end_rel) = template[search_start..].find(&close_tag) {
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
                let base_key = key.split('|').next().unwrap_or(key).trim();

                if preserve_unresolved
                    && !is_literal_operand(base_key)
                    && !context.contains_key(base_key)
                {
                    result.push_str(&template[pos..end + 2]);
                    cursor = end + 2;
                    continue;
                }

                let value = if let Some((base, chain)) = key.split_once('|') {
                    crate::modifier::apply(resolve_operand(base.trim(), context), chain)
                } else {
                    format_value(&resolve_operand(key, context))
                };

                let final_value = if in_quotes[pos] { escape_html_attribute(&value) } else { value };
                result.push_str(&final_value);
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
                let close_tag = format!("{{/{}}}", tag);
                if let Some(end_rel) = template[header_end..].find(&close_tag) {
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

        let rendered = match tag {
            "if" => evaluate_if_block(inner, expression, context, preserve_unresolved),
            "for" => evaluate_for_block(inner, expression, context, preserve_unresolved),
            "unless" => evaluate_unless_block(inner, expression, context, preserve_unresolved),
            "switch" => evaluate_switch_block(inner, expression, context, preserve_unresolved),
            "capture" => {
                evaluate_capture(inner, expression, context, preserve_unresolved);
                String::new()
            }
            "with" => evaluate_with(inner, expression, context, preserve_unresolved),
            _ => String::new(),
        };

        result.push_str(&rendered);
        cursor = block_end;
    }

    result
}

fn strip_flow_markers(s: &str) -> String {
    if !s.contains(BREAK_MARKER) && !s.contains(CONTINUE_MARKER) {
        return s.to_string();
    }
    s.replace(BREAK_MARKER, "").replace(CONTINUE_MARKER, "")
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
        "data" | "meta" | "config"
    )
}