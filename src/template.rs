use crate::state::PROP_RE;
use serde_json::Value;
use std::collections::HashMap;

pub fn get_nested_value(
    path: &str,
    context: &HashMap<String, Value>,
) -> Value {
    let path = path.trim();

    if path.is_empty() {
        return Value::Null;
    }

    let parts: Vec<&str> = path.split('.').collect();
    let mut current = context.get(parts[0]);

    for part in &parts[1..] {
        match current {
            Some(Value::Object(map)) => current = map.get(*part),
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

pub fn evaluate_condition(
    expr: &str,
    context: &HashMap<String, Value>,
) -> bool {
    let expr = expr.trim();

    if expr.is_empty() {
        return false;
    }

    if let Some(stripped) = expr.strip_prefix('!') {
        return !evaluate_condition(stripped.trim(), context);
    }

    for op in ["==", "!=", "<=", ">=", "<", ">"] {
        if let Some(pos) = expr.find(op) {
            let left = resolve_operand(&expr[..pos], context);
            let right = resolve_operand(
                &expr[pos + op.len()..],
                context,
            );

            return compare_values(&left, &right, op);
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
        return Value::String(
            trimmed[1..trimmed.len() - 1].to_string()
        );
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

fn compare_values(
    left: &Value,
    right: &Value,
    op: &str,
) -> bool {
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
    let mut best = None;

    for token in [
        "{{#for ", "{{for ", "{{#if ", "{{if ",
        "{for ", "{if ", "{{", "<script", "</script>",
    ] {
        if let Some(pos) = template[from..].find(token) {
            let absolute = from + pos;

            if best.map(|(p, _)| absolute < p).unwrap_or(true) {
                best = Some((absolute, token));
            }
        }
    }

    best
}

pub fn find_block_end(
    template: &str,
    start: usize,
    token: &str,
) -> Option<(usize, usize)> {
    let is_double = token.starts_with("{{");
    let kind = if token.contains("for") { "for" } else { "if" };

    let header_end = if is_double {
        template[start..].find("}}")? + start + 2
    } else {
        template[start..].find('}')? + start + 1
    };

    let open_patterns = if is_double {
        vec![
            format!("{{{{#{kind} "),
            format!("{{{{{kind} "),
        ]
    } else {
        vec![format!("{{{kind} ")]
    };

    let close_patterns = if is_double {
        vec![
            format!("{{{{/#{kind}}}}}"),
            format!("{{{{/{kind}}}}}"),
        ]
    } else {
        vec![format!("{{/{kind}}}")]
    };

    let mut depth = 1usize;
    let mut cursor = header_end;

    while cursor < template.len() {
        let mut next_open = None;

        for pat in &open_patterns {
            if let Some(p) = template[cursor..].find(pat) {
                let abs = cursor + p;

                if next_open
                    .map(|(op, _)| abs < op)
                    .unwrap_or(true)
                {
                    next_open = Some((abs, pat.len()));
                }
            }
        }

        let mut next_close = None;

        for pat in &close_patterns {
            if let Some(p) = template[cursor..].find(pat) {
                let abs = cursor + p;

                if next_close
                    .map(|(cp, _)| abs < cp)
                    .unwrap_or(true)
                {
                    next_close = Some((abs, pat.len()));
                }
            }
        }

        match (next_open, next_close) {
            (Some((o, len)), Some((c, _))) if o < c => {
                depth += 1;
                cursor = o + len;
            }
            (_, Some((c, len))) => {
                depth -= 1;

                if depth == 0 {
                    return Some((header_end, c + len));
                }

                cursor = c + len;
            }
            _ => return None,
        }
    }

    None
}

pub fn split_else_branches(
    inner: &str,
) -> (String, Option<String>) {
    let mut depth = 0usize;
    let mut cursor = 0usize;

    let targets = [
        ("{{#if ", 0), ("{{if ", 0), ("{if ", 0),
        ("{{#for ", 0), ("{{for ", 0), ("{for ", 0),
        ("{{/#if}}", 1), ("{{/if}}", 1), ("{/if}", 1),
        ("{{/#for}}", 1), ("{{/for}}", 1), ("{/for}", 1),
        ("{{else if ", 2), ("{else if ", 2),
        ("{{#else}}", 3), ("{{else}}", 3), ("{else}", 3),
    ];

    while cursor < inner.len() {
        let mut candidate = None;

        for (pat, kind) in targets {
            if let Some(p) = inner[cursor..].find(pat) {
                let abs = cursor + p;

                if candidate
                    .map(|(cp, _, _)| abs < cp)
                    .unwrap_or(true)
                {
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
                return (
                    inner[..pos].to_string(),
                    Some(inner[pos..].to_string()),
                );
            }
            _ => cursor = pos + len,
        }
    }

    (inner.to_string(), None)
}

pub fn evaluate_if_block(
    inner: &str,
    expression: &str,
    context: &HashMap<String, Value>,
) -> String {
    let (true_part, else_part) =
        split_else_branches(inner);

    if evaluate_condition(expression, context) {
        return render_control_flow(
            &true_part,
            context,
        );
    }

    if let Some(rest) = else_part {
        let stripped = rest
            .strip_prefix("{{else if ")
            .or_else(|| rest.strip_prefix("{else if "));

        if let Some(v) = stripped {
            let delimiter =
                if rest.starts_with("{{") { "}}" } else { "}" };

            if let Some(end) = v.find(delimiter) {
                let expr = v[..end].trim();
                let body = &v[end + delimiter.len()..];

                return evaluate_if_block(
                    body,
                    expr,
                    context,
                );
            }
        }

        let else_body = rest
            .strip_prefix("{{#else}}")
            .or_else(|| rest.strip_prefix("{{else}}"))
            .or_else(|| rest.strip_prefix("{else}"))
            .unwrap_or(&rest);

        return render_control_flow(
            else_body,
            context,
        );
    }

    String::new()
}

pub fn evaluate_for_block(
    inner: &str,
    expression: &str,
    context: &HashMap<String, Value>,
) -> String {
    let parts: Vec<&str> =
        expression.split_whitespace().collect();

    if parts.len() != 3 || parts[1] != "in" {
        return String::new();
    }

    let item_var = parts[0];
    let array = get_nested_value(parts[2], context);
    let (body, else_part) =
        split_else_branches(inner);

    let Value::Array(items) = array else {
        return render_else(else_part, context);
    };

    if items.is_empty() {
        return render_else(else_part, context);
    }

    let mut result =
        String::with_capacity(body.len() * items.len());

    for (idx, item) in items.iter().enumerate() {
        let mut child = context.clone();

        child.insert(
            item_var.to_string(),
            item.clone(),
        );
        child.insert(
            "@index".to_string(),
            Value::Number((idx as i64).into()),
        );
        child.insert(
            "@number".to_string(),
            Value::Number(((idx + 1) as i64).into()),
        );
        child.insert(
            "@first".to_string(),
            Value::Bool(idx == 0),
        );
        child.insert(
            "@last".to_string(),
            Value::Bool(idx + 1 == items.len()),
        );

        result.push_str(
            &render_control_flow(&body, &child)
        );
    }

    result
}

fn render_else(
    else_part: Option<String>,
    context: &HashMap<String, Value>,
) -> String {
    else_part
        .map(|v| {
            let body = v
                .strip_prefix("{{#else}}")
                .or_else(|| v.strip_prefix("{{else}}"))
                .or_else(|| v.strip_prefix("{else}"))
                .unwrap_or(&v);

            render_control_flow(body, context)
        })
        .unwrap_or_default()
}

pub fn render_control_flow(
    template: &str,
    context: &HashMap<String, Value>,
) -> String {
    let mut result =
        String::with_capacity(template.len());
    let mut cursor = 0usize;

    while cursor < template.len() {
        let Some((pos, token)) =
            find_next_token(template, cursor)
        else {
            result.push_str(&template[cursor..]);
            break;
        };

        if pos > cursor {
            result.push_str(&template[cursor..pos]);
        }

        if token == "<script" {
            if let Some(end_rel) =
                template[pos..].find("</script>")
            {
                let end =
                    pos + end_rel + "</script>".len();

                result.push_str(
                    &template[pos..end]
                );

                cursor = end;
                continue;
            }

            result.push_str(&template[pos..]);
            break;
        }

        if token == "{{" {
            if let Some(end_rel) =
                template[pos + 2..].find("}}")
            {
                let end =
                    pos + 2 + end_rel;

                let key =
                    template[pos + 2..end].trim();

                let value = format_value(
                    &get_nested_value(key, context)
                );

                let before = &template[..pos];
                let mut quote = None;

                for ch in before.chars() {
                    match quote {
                        Some(active) if ch == active => {
                            quote = None;
                        }
                        None if ch == '"' || ch == '\'' => {
                            quote = Some(ch);
                        }
                        _ => {}
                    }
                }

                if quote.is_some() {
                    result.push_str(
                        &escape_html_attribute(&value)
                    );
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

        let is_double = token.starts_with("{{");
        let kind =
            if token.contains("for") { "for" } else { "if" };

        let header_end = if is_double {
            match template[pos..].find("}}") {
                Some(v) => pos + v,
                None => {
                    result.push_str(&template[pos..]);
                    break;
                }
            }
        } else {
            match template[pos..].find('}') {
                Some(v) => pos + v,
                None => {
                    result.push_str(&template[pos..]);
                    break;
                }
            }
        };

        let expression =
            template[pos + token.len()..header_end]
                .trim();

        let Some((content_start, block_end)) =
            find_block_end(template, pos, token)
        else {
            result.push_str(&template[pos..]);
            break;
        };

        let closing_len = if is_double {
            let slice = &template[..block_end];

            slice.len()
                - slice.rfind("{{").unwrap_or(slice.len())
        } else {
            kind.len() + 3
        };

        let inner =
            &template[content_start..block_end - closing_len];

        let rendered = if kind == "for" {
            evaluate_for_block(
                inner,
                expression,
                context,
            )
        } else {
            evaluate_if_block(
                inner,
                expression,
                context,
            )
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

            let value = format_value(
                &get_nested_value(key, context)
            );

            let before = &template[..full.start()];
            let mut quote = None;

            for ch in before.chars() {
                match quote {
                    Some(active) if ch == active => {
                        quote = None;
                    }
                    None if ch == '"' || ch == '\'' => {
                        quote = Some(ch);
                    }
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

pub fn clean_empty_tags(html: &str) -> String {
    let mut result =
        String::with_capacity(html.len());

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