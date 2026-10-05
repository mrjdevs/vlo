use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{cmp::Ordering, collections::HashMap, fs, path::Path};

pub struct HandlerOutput {
    pub data: Vec<Value>,
    pub meta: HashMap<String, Value>,
    pub config: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HandlerRules {
    #[serde(default)]
    pub transforms: Vec<TransformRule>,
    #[serde(default)]
    pub meta: HashMap<String, MetaRule>,
    #[serde(default)]
    pub computed: HashMap<String, ComputedRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransformRule {
    pub op: String,
    #[serde(default)] pub key: Option<String>,
    #[serde(default)] pub value: Option<String>,
    #[serde(default)] pub field: Option<String>,
    #[serde(default)] pub count: Option<String>,
    #[serde(default)] pub order: Option<String>,
    #[serde(default)] pub agg: Option<String>,
    #[serde(default)] pub from: Option<String>,
    #[serde(default)] pub to: Option<String>,
    #[serde(default)] pub when: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaRule {
    pub op: String,
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComputedRule {
    pub op: String,
    pub field: String,
    #[serde(default)] pub against: Option<String>,
    #[serde(default)] pub base: Option<f64>,
    #[serde(default)] pub order: Option<String>,
}

pub fn execute_module_handler(
    module_path: &Path,
    props: &HashMap<String, Value>,
    raw_data: Vec<Value>,
) -> Result<HandlerOutput, String> {
    let rules = load_handler_rules(module_path)?;

    let mut data = raw_data;
    for rule in rules.transforms.iter() {
        data = apply_transform(rule, data, props);
    }

    let mut meta = HashMap::new();
    for (name, rule) in &rules.meta {
        meta.insert(name.clone(), compute_meta_field(rule, &data));
    }

    apply_computed_fields(&mut data, &rules.computed, &meta);
    apply_selected_computed_value(&mut data, props);

    // Recalculate meta if a computed field was promoted to "value"
    if props.contains_key("computed") {
        meta.clear();
        for (name, rule) in &rules.meta {
            meta.insert(name.clone(), compute_meta_field(rule, &data));
        }
    }

    let config = props.clone();
    Ok(HandlerOutput { data, meta, config })
}

fn apply_selected_computed_value(data: &mut [Value], props: &HashMap<String, Value>) {
    let Some(computed) = props.get("computed").and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()) else {
        return;
    };

    for item in data {
        let Some(obj) = item.as_object_mut() else { continue; };
        if let Some(computed_val) = obj.get(computed).cloned() {
            if !computed_val.is_null() {
                obj.insert("display_value".to_string(), computed_val.clone());
                obj.insert("value".to_string(), computed_val);
            }
        }
    }
}

fn load_handler_rules(module_path: &Path) -> Result<HandlerRules, String> {
    let manifest_path = module_path.join("module.json");
    let manifest_str = fs::read_to_string(&manifest_path).map_err(|e| format!("Cannot read module.json: {}", e))?;
    let manifest: Value = serde_json::from_str(&manifest_str).map_err(|e| format!("Invalid module.json: {}", e))?;

    match manifest.get("handler") {
        Some(handler) => serde_json::from_value(handler.clone()).map_err(|e| format!("Invalid handler rules: {}", e)),
        None => Ok(HandlerRules::default()),
    }
}

fn apply_transform(rule: &TransformRule, data: Vec<Value>, props: &HashMap<String, Value>) -> Vec<Value> {
    if let Some(when) = &rule.when {
        let condition = resolve_template(when, props);
        if condition.is_empty() || condition == "false" || condition == "0" || condition == "null" {
            return data;
        }
    }

    match rule.op.as_str() {
        "group" => {
            let key_field = resolve_template(rule.key.as_deref().unwrap_or(""), props);
            if key_field.is_empty() || key_field == "{{group_by}}" { return data; }
            let value_field = resolve_template(rule.value.as_deref().unwrap_or("value"), props);
            let agg = rule.agg.as_deref().unwrap_or("sum");
            group_by(&data, &key_field, &value_field, agg)
        }
        "sort" => {
            let key = resolve_template(rule.key.as_deref().unwrap_or(""), props);
            if key.is_empty() { return data; }
            let order = resolve_template(rule.order.as_deref().unwrap_or("asc"), props);
            sort_by(&data, &key, &order)
        }
        "limit" => {
            let count = resolve_template(rule.count.as_deref().unwrap_or("10"), props).parse::<usize>().unwrap_or(10);
            data.into_iter().take(count).collect()
        }
        "filter" => {
            let field = resolve_template(rule.field.as_deref().unwrap_or(""), props);
            let value = resolve_template(rule.value.as_deref().unwrap_or(""), props);
            filter_by(&data, &field, &value)
        }
        "map" => {
            let from = resolve_template(rule.from.as_deref().unwrap_or(""), props);
            let to = resolve_template(rule.to.as_deref().unwrap_or(&from), props);
            
            if let Some((date_field, period)) = from.split_once('|') {
                let period = period.trim();
                if matches!(period.to_lowercase().as_str(), "year" | "month" | "week" | "quarter" | "qtr" | "day") {
                    return map_date_field(&data, date_field.trim(), period, &to);
                }
            }
            map_field(&data, &from, &to)
        }
        "date_extract" => {
            let field = resolve_template(rule.field.as_deref().unwrap_or(""), props);
            let period = resolve_template(rule.value.as_deref().unwrap_or("month"), props);
            let to = resolve_template(rule.to.as_deref().unwrap_or(&field), props);
            map_date_field(&data, &field, &period, &to)
        }
        _ => data,
    }
}

fn map_date_field(data: &[Value], from: &str, period: &str, to: &str) -> Vec<Value> {
    data.iter().map(|row| {
        let mut new_row = row.clone();
        if let Some(obj) = new_row.as_object_mut() {
            let date_value = obj.get(from).map(value_to_string).unwrap_or_default();
            obj.insert(to.to_string(), Value::String(extract_date_period(&date_value, period)));
        }
        new_row
    }).collect()
}

fn resolve_template(template: &str, props: &HashMap<String, Value>) -> String {
    let mut result = template.to_string();
    for (key, value) in props {
        let placeholder = format!("{{{{{}}}}}", key);
        if result.contains(&placeholder) {
            result = result.replace(&placeholder, &value_to_string(value));
        }
    }
    loop {
        let Some(start) = result.find("{{") else { break; };
        let Some(end_rel) = result[start + 2..].find("}}") else { break; };
        let end = start + 2 + end_rel;
        let expression = &result[start + 2..end];

        if let Some((name, default)) = expression.split_once('|') {
            let replacement = props.get(name.trim())
                .filter(|v| !v.is_null() && !value_to_string(v).is_empty())
                .map(|v| value_to_string(v))
                .unwrap_or_else(|| default.trim().to_string());
            result.replace_range(start..end + 2, &replacement);
        } else {
            break;
        }
    }
    result
}

fn extract_date_period(date_str: &str, period: &str) -> String {
    let date_str = date_str.trim().trim_matches('"');
    let date_part = date_str.split(|c| c == 'T' || c == ' ').next().unwrap_or("");
    let parts: Vec<&str> = date_part.split('-').collect();
    if parts.len() < 3 { return "Unknown".to_string(); }

    let Ok(year) = parts[0].parse::<i32>() else { return "Unknown".to_string() };
    let Ok(month) = parts[1].parse::<u32>() else { return "Unknown".to_string() };
    let Ok(day) = parts[2].parse::<u32>() else { return "Unknown".to_string() };

    match period.trim().to_lowercase().as_str() {
        "year" => format!("{}", year),
        "month" => format!("{}-{:02}", year, month),
        "week" => format!("{}-W{:02}", year, iso_week_number(year, month, day)),
        "quarter" | "qtr" => format!("{}-Q{}", year, (month - 1) / 3 + 1),
        "day" => format!("{}-{:02}-{:02}", year, month, day),
        _ => date_str.to_string(),
    }
}

fn iso_week_number(year: i32, month: u32, day: u32) -> u32 {
    let days_in_months = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let mut day_of_year: u32 = days_in_months[..month as usize].iter().sum();
    day_of_year += day;
    if is_leap && month > 2 { day_of_year += 1; }
    let jan1_weekday = (year + (year - 1) / 4 - (year - 1) / 100 + (year - 1) / 400) % 7;
    let week = (day_of_year as i32 + jan1_weekday as i32 - 1) / 7 + 1;
    week.max(1).min(53) as u32
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        Value::Number(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

fn group_by(data: &[Value], key_field: &str, value_field: &str, agg: &str) -> Vec<Value> {
    let mut groups: HashMap<String, Vec<f64>> = HashMap::new();
    for row in data {
        if let Some(obj) = row.as_object() {
            let key = obj.get(key_field).map(value_to_string).filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "Other".to_string());
            let value = obj.get(value_field).and_then(|v| v.as_f64()).unwrap_or(0.0);
            groups.entry(key).or_default().push(value);
        }
    }

    let mut result: Vec<Value> = groups.into_iter().map(|(label, values)| {
        let aggregated = match agg {
            "avg" => values.iter().sum::<f64>() / values.len().max(1) as f64,
            "count" => values.len() as f64,
            "min" => values.iter().copied().fold(f64::INFINITY, f64::min),
            "max" => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            _ => values.iter().sum::<f64>(),
        };
        serde_json::json!({"label": label, "value": safe_number(aggregated), "count": values.len()})
    }).collect();

    result.sort_by(|a, b| number_field(b, "value").partial_cmp(&number_field(a, "value")).unwrap_or(Ordering::Equal));
    result
}

fn sort_by(data: &[Value], key: &str, order: &str) -> Vec<Value> {
    let mut result = data.to_vec();
    result.sort_by(|a, b| {
        let cmp = compare_values(&get_nested(a, key), &get_nested(b, key));
        if order.eq_ignore_ascii_case("desc") { cmp.reverse() } else { cmp }
    });
    result
}

fn filter_by(data: &[Value], field: &str, value: &str) -> Vec<Value> {
    data.iter().filter(|row| row.as_object().and_then(|obj| obj.get(field)).map(|v| value_to_string(v) == value).unwrap_or(false)).cloned().collect()
}

fn map_field(data: &[Value], from: &str, to: &str) -> Vec<Value> {
    data.iter().map(|row| {
        let mut new_row = row.clone();
        if let Some(obj) = new_row.as_object_mut() {
            let value = obj.get(from).cloned().unwrap_or(Value::Null);
            let final_value = if value.is_null() || (value.is_string() && value.as_str().unwrap_or("").trim().is_empty()) {
                Value::String("Other".to_string())
            } else { value };
            obj.insert(to.to_string(), final_value);
        }
        new_row
    }).collect()
}

fn compute_meta_field(rule: &MetaRule, data: &[Value]) -> Value {
    let values: Vec<f64> = data.iter().filter_map(|row| row.as_object().and_then(|obj| obj.get(&rule.field)).and_then(|v| v.as_f64())).collect();
    if values.is_empty() { return serde_json::json!(0); }

    match rule.op.as_str() {
        "sum" => serde_json::json!(safe_number(values.iter().sum())),
        "avg" => serde_json::json!(safe_number(values.iter().sum::<f64>() / values.len() as f64)),
        "min" => serde_json::json!(safe_number(values.iter().copied().fold(f64::INFINITY, f64::min))),
        "max" => serde_json::json!(safe_number(values.iter().copied().fold(f64::NEG_INFINITY, f64::max))),
        "count" => serde_json::json!(values.len()),
        _ => Value::Null,
    }
}

fn apply_computed_fields(data: &mut Vec<Value>, rules: &HashMap<String, ComputedRule>, meta: &HashMap<String, Value>) {
    if data.is_empty() || rules.is_empty() { return; }

    let values: Vec<f64> = data.iter().map(|item| item.as_object().and_then(|obj| obj.get("value")).and_then(|v| v.as_f64()).unwrap_or(0.0)).collect();
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let total = values.iter().sum::<f64>();
    let mean = if values.is_empty() { 0.0 } else { total / values.len() as f64 };
    let variance = if values.is_empty() { 0.0 } else { values.iter().map(|v| (*v - mean).powi(2)).sum::<f64>() / values.len() as f64 };
    let stddev = variance.sqrt();

    for (index, item) in data.iter_mut().enumerate() {
        let Some(obj) = item.as_object_mut() else { continue; };
        for (name, rule) in rules {
            obj.insert(name.clone(), compute_field_for_item(rule, obj, index, &values, meta, max, min, total, mean, stddev));
        }
    }
}

fn compute_field_for_item(rule: &ComputedRule, item: &serde_json::Map<String, Value>, index: usize, values: &[f64], meta: &HashMap<String, Value>, max: f64, min: f64, total: f64, mean: f64, stddev: f64) -> Value {
    let value = item.get(&rule.field).and_then(|v| v.as_f64()).unwrap_or(0.0);

    match rule.op.as_str() {
        "percent_of_max" => number_json(if max > 0.0 { value / max * 100.0 } else { 0.0 }),
        "percent_of_total" => number_json(if total != 0.0 { value / total * 100.0 } else { 0.0 }),
        "rank" => {
            let order = rule.order.as_deref().unwrap_or("desc");
            let rank = if order.eq_ignore_ascii_case("asc") { 1 + values.iter().filter(|v| **v < value).count() } else { 1 + values.iter().filter(|v| **v > value).count() };
            serde_json::json!(rank)
        }
        "cumulative_sum" => number_json(values.iter().take(index + 1).sum()),
        "running_avg" => { let slice = &values[..=index]; number_json(slice.iter().sum::<f64>() / slice.len() as f64) }
        "normalize" => number_json(if max > min { (value - min) / (max - min) * 100.0 } else { 100.0 }),
        "ratio" => { let against = resolve_against(rule, item, meta); number_json(if against != 0.0 { value / against } else { 0.0 }) }
        "difference" => number_json(value - if index > 0 { values[index - 1] } else { 0.0 }),
        "percent_change" => {
            if index == 0 { number_json(0.0) } else {
                let prev = values[index - 1];
                number_json(if prev != 0.0 { (value - prev) / prev * 100.0 } else { 0.0 })
            }
        }
        "distance_from_min" => number_json(value - min),
        "distance_from_max" => number_json(max - value),
        "index" => {
            let base = rule.base.unwrap_or(100.0);
            let first = values.first().copied().unwrap_or(0.0);
            number_json(if first != 0.0 { value / first * base } else { 0.0 })
        }
        "z_score" => number_json(if stddev > 0.0 { (value - mean) / stddev } else { 0.0 }),
        "is_max" => serde_json::json!((value - max).abs() < f64::EPSILON),
        "is_min" => serde_json::json!((value - min).abs() < f64::EPSILON),
        "is_positive" => serde_json::json!(value > 0.0),
        "is_negative" => serde_json::json!(value < 0.0),
        _ => Value::Null,
    }
}

fn resolve_against(rule: &ComputedRule, item: &serde_json::Map<String, Value>, meta: &HashMap<String, Value>) -> f64 {
    let Some(against) = rule.against.as_deref() else { return 0.0; };
    if let Some(value) = item.get(against).and_then(|v| v.as_f64()) { return value; }
    meta.get(against).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn number_field(value: &Value, field: &str) -> f64 { value.as_object().and_then(|obj| obj.get(field)).and_then(|v| v.as_f64()).unwrap_or(0.0) }
fn number_json(value: f64) -> Value { serde_json::Number::from_f64(value).map(Value::Number).unwrap_or_else(|| serde_json::json!(0)) }
fn safe_number(value: f64) -> f64 { if value.is_finite() { value } else { 0.0 } }

fn get_nested(value: &Value, path: &str) -> Value {
    let mut current = value.clone();
    for part in path.split('.') {
        match current {
            Value::Object(map) => current = map.get(part).cloned().unwrap_or(Value::Null),
            _ => return Value::Null,
        }
    }
    current
}

fn compare_values(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64().unwrap_or(0.0).partial_cmp(&b.as_f64().unwrap_or(0.0)).unwrap_or(Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => Ordering::Equal,
    }
}