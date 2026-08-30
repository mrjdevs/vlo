use crate::state::PROP_RE;
use serde_json::Value;
use std::collections::HashMap;

// ============================================================
// 17. VLO TEMPLATE ENGINE
// ============================================================

pub fn get_nested_value(
    path: &str,
    context: &HashMap<String, Value>,
) -> Value {
    let parts: Vec<&str> = path.split('.').collect();

    let mut val = Value::Object(
        context
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );

    for part in parts {
        if let Value::Object(map) = val {
            val = map.get(part).cloned().unwrap_or(Value::Null);
        } else {
            return Value::Null;
        }
    }

    val
}

pub fn evaluate_condition(
    expr: &str,
    context: &HashMap<String, Value>,
) -> bool {
    let expr = expr.trim();
    let operators = ["==", "!=", "<=", ">=", "<", ">"];

    for op in operators {
        if let Some(pos) = expr.find(op) {
            let left = expr[..pos].trim();
            let right = expr[pos + op.len()..].trim();

            let left_val = resolve_operand(left, context);
            let right_val = resolve_operand(right, context);

            return compare_values(&left_val, &right_val, op);
        }
    }

    is_truthy(&resolve_operand(expr, context))
}

fn resolve_operand(
    operand: &str,
    context: &HashMap<String, Value>,
) -> Value {
    let trimmed = operand.trim();

    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        Value::String(trimmed[1..trimmed.len() - 1].to_string())
    } else if let Ok(num) = trimmed.parse::<i64>() {
        Value::Number(num.into())
    } else if let Ok(num) = trimmed.parse::<f64>() {
        serde_json::Number::from_f64(num)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    } else {
        get_nested_value(trimmed, context)
    }
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

pub fn find_next_token(
    template: &str,
    from: usize,
) -> Option<(usize, &'static str)> {
    let mut best: Option<(usize, &'static str)> = None;

    for token in ["{for ", "{if ", "{{", "<script", "</script>"] {
        if let Some(pos) = template[from..].find(token) {
            let absolute = from + pos;
            if best.map(|(p, _)| absolute < p).unwrap_or(true) {
                best = Some((absolute, token));
            }
        }
    }

    best
}

pub fn find_modern_block_end(
    template: &str,
    start: usize,
    kind: &str,
) -> Option<(usize, usize)> {
    let open = format!("{{{} ", kind);
    let close = format!("{{/{}}}", kind);
    let mut depth = 1usize;

    let header_end = template[start..].find('}')? + start + 1;
    let mut cursor = header_end;

    while cursor < template.len() {
        let next_open = template[cursor..]
            .find(&open)
            .map(|p| cursor + p);
        let next_close = template[cursor..]
            .find(&close)
            .map(|p| cursor + p);

        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                cursor = o + open.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    return Some((header_end, c + close.len()));
                }
                cursor = c + close.len();
            }
            _ => return None,
        }
    }

    None
}

pub fn split_modern_else(inner: &str) -> (String, Option<String>) {
    let mut depth = 0usize;
    let mut cursor = 0usize;

    while cursor < inner.len() {
        let next_if = inner[cursor..].find("{if ").map(|p| cursor + p);
        let next_close = inner[cursor..].find("{/if}").map(|p| cursor + p);
        let next_else = inner[cursor..].find("{else}").map(|p| cursor + p);
        let next_else_if = inner[cursor..].find("{else if ").map(|p| cursor + p);

        let mut candidates = Vec::new();
        if let Some(p) = next_if {
            candidates.push((p, 0));
        }
        if let Some(p) = next_close {
            candidates.push((p, 1));
        }
        if let Some(p) = next_else_if {
            candidates.push((p, 2));
        }
        if let Some(p) = next_else {
            candidates.push((p, 3));
        }

        let Some((pos, kind)) = candidates.into_iter().min_by_key(|x| x.0) else {
            break;
        };

        match kind {
            0 => {
                depth += 1;
                cursor = pos + 4;
            }
            1 => {
                if depth > 0 {
                    depth -= 1;
                }
                cursor = pos + 5;
            }
            2 | 3 if depth == 0 => {
                return (
                    inner[..pos].to_string(),
                    Some(inner[pos..].to_string()),
                );
            }
            _ => {
                cursor = pos + if kind == 2 { 10 } else { 6 };
            }
        }
    }

    (inner.to_string(), None)
}

pub fn split_for_else(inner: &str) -> (String, Option<String>) {
    let mut depth_for = 0usize;
    let mut depth_if = 0usize;
    let mut cursor = 0usize;

    while cursor < inner.len() {
        let mut candidates = Vec::new();
        if let Some(p) = inner[cursor..].find("{for ") {
            candidates.push((cursor + p, 0));
        }
        if let Some(p) = inner[cursor..].find("{/for}") {
            candidates.push((cursor + p, 1));
        }
        if let Some(p) = inner[cursor..].find("{if ") {
            candidates.push((cursor + p, 2));
        }
        if let Some(p) = inner[cursor..].find("{/if}") {
            candidates.push((cursor + p, 3));
        }
        if let Some(p) = inner[cursor..].find("{else}") {
            candidates.push((cursor + p, 4));
        }
        if let Some(p) = inner[cursor..].find("{else if ") {
            candidates.push((cursor + p, 5));
        }

        let Some((pos, kind)) = candidates.into_iter().min_by_key(|x| x.0) else {
            break;
        };

        match kind {
            0 => {
                depth_for += 1;
                cursor = pos + 5;
            }
            1 => {
                if depth_for > 0 {
                    depth_for -= 1;
                }
                cursor = pos + 6;
            }
            2 => {
                depth_if += 1;
                cursor = pos + 4;
            }
            3 => {
                if depth_if > 0 {
                    depth_if -= 1;
                }
                cursor = pos + 5;
            }
            4 | 5 if depth_for == 0 && depth_if == 0 => {
                return (
                    inner[..pos].to_string(),
                    Some(inner[pos..].to_string()),
                );
            }
            _ => {
                cursor = pos + if kind == 5 { 10 } else { 6 };
            }
        }
    }

    (inner.to_string(), None)
}

pub fn evaluate_modern_if(
    inner: &str,
    expression: &str,
    context: &HashMap<String, Value>,
) -> String {
    let (true_part, else_part) = split_modern_else(inner);

    if evaluate_condition(expression, context) {
        return render_control_flow(&true_part, context);
    }

    if let Some(rest) = else_part {
        if let Some(v) = rest.strip_prefix("{else if ") {
            if let Some(end) = v.find('}') {
                let expr = v[..end].trim();
                return evaluate_modern_if(&v[end + 1..], expr, context);
            }
        }

        return render_control_flow(
            rest.strip_prefix("{else}").unwrap_or(&rest),
            context,
        );
    }

    String::new()
}

pub fn evaluate_modern_for(
    inner: &str,
    expression: &str,
    context: &HashMap<String, Value>,
) -> String {
    let parts: Vec<&str> = expression.split_whitespace().collect();
    if parts.len() != 3 || parts[1] != "in" {
        return String::new();
    }

    let item_var = parts[0];
    let array_path = parts[2];
    let array = get_nested_value(array_path, context);

    let (body, else_part) = split_for_else(inner);

    let Value::Array(items) = array else {
        return else_part
            .map(|v| {
                render_control_flow(
                    v.strip_prefix("{else}").unwrap_or(&v),
                    context,
                )
            })
            .unwrap_or_default();
    };

    if items.is_empty() {
        return else_part
            .map(|v| {
                render_control_flow(
                    v.strip_prefix("{else}").unwrap_or(&v),
                    context,
                )
            })
            .unwrap_or_default();
    }

    let mut result = String::with_capacity(body.len() * items.len());

    for (idx, item) in items.iter().enumerate() {
        let mut child = context.clone();
        child.insert(item_var.to_string(), item.clone());
        child.insert("@index".to_string(), Value::Number((idx as i64).into()));
        child.insert("@number".to_string(), Value::Number(((idx + 1) as i64).into()));
        child.insert("@first".to_string(), Value::Bool(idx == 0));
        child.insert("@last".to_string(), Value::Bool(idx + 1 == items.len()));

        result.push_str(&render_control_flow(&body, &child));
    }

    result
}

pub fn render_control_flow(
    template: &str,
    context: &HashMap<String, Value>,
) -> String {
    let mut result = String::with_capacity(template.len());
    let mut cursor = 0usize;

    while cursor < template.len() {
        let Some((pos, token)) = find_next_token(template, cursor) else {
            result.push_str(&template[cursor..]);
            break;
        };

        if pos > cursor {
            result.push_str(&template[cursor..pos]);
        }

        if token == "<script" {
            if let Some(end_rel) = template[pos..].find("</script>") {
                let end = pos + end_rel + "</script>".len();
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
                let value = format_value(&get_nested_value(key, context));

                let before = &template[..pos];
                let mut quote: Option<char> = None;
                for ch in before.chars() {
                    match quote {
                        Some(active) if ch == active => quote = None,
                        None if ch == '"' || ch == '\'' => quote = Some(ch),
                        _ => {}
                    }
                }

                if quote.is_some() {
                    result.push_str(&escape_html_attribute(&value));
                } else {
                    result.push_str(&value);
                }

                cursor = end + 2;
            } else {
                result.push_str(&template[pos..]);
                break;
            }

            continue;
        }

        let kind = if token == "{for " { "for" } else { "if" };
        let header_end = match template[pos..].find('}') {
            Some(v) => pos + v,
            None => {
                result.push_str(&template[pos..]);
                break;
            }
        };

        let expression = template[pos + kind.len() + 2..header_end].trim();
        let Some((content_start, block_end)) = find_modern_block_end(template, pos, kind) else {
            result.push_str(&template[pos..]);
            break;
        };

        let inner = &template[content_start..block_end - (kind.len() + 3)];
        let rendered = if kind == "for" {
            evaluate_modern_for(inner, expression, context)
        } else {
            evaluate_modern_if(inner, expression, context)
        };

        result.push_str(&rendered);
        cursor = block_end;
    }

    result
}

pub fn render_interpolations(
    template: &str,
    context: &HashMap<String, Value>,
) -> String {
    PROP_RE
        .replace_all(template, |captures: &regex::Captures| {
            let full = captures.get(0).unwrap();
            let key = captures[1].trim();
            let value = format_value(&get_nested_value(key, context));

            let before = &template[..full.start()];
            let mut quote: Option<char> = None;
            for ch in before.chars() {
                match quote {
                    Some(active) if ch == active => quote = None,
                    None if ch == '"' || ch == '\'' => quote = Some(ch),
                    _ => {}
                }
            }

            if quote.is_some() {
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

pub fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}