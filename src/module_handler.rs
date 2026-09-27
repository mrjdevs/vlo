use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fs, path::Path};

/// Generic handler output that any module component can consume
pub struct HandlerOutput {
    pub data: Vec<Value>,
    pub meta: HashMap<String, Value>,
    pub config: HashMap<String, Value>,
}

/// Handler rule definitions from module.json
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HandlerRules {
    #[serde(default)]
    pub transforms: Vec<TransformRule>,
    #[serde(default)]
    pub meta: HashMap<String, MetaRule>,
    #[serde(default)]
    pub computed: HashMap<String, ComputedRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformRule {
    pub op: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub count: Option<String>,
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub agg: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaRule {
    pub op: String,
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputedRule {
    pub op: String,
    pub field: String,
}

/// Execute a module's handler pipeline
pub fn execute_module_handler(
    module_path: &Path,
    props: &HashMap<String, Value>,
    raw_data: Vec<Value>,
) -> Result<HandlerOutput, String> {
    
    // Load handler rules from module.json
    let rules = load_handler_rules(module_path)?;
    
    crate::vlo_debug!("🔧 Module handler: {} transforms, {} meta rules, {} computed rules", 
        rules.transforms.len(), rules.meta.len(), rules.computed.len());
    
    crate::vlo_debug!("🔧 Props: {:?}", props);
    crate::vlo_debug!("🔧 Raw data (first 2 items): {:?}", &raw_data[..raw_data.len().min(2)]);
    
    // Apply transforms sequentially
    let mut data = raw_data;
    for (i, rule) in rules.transforms.iter().enumerate() {
        crate::vlo_debug!("🔧 Transform {}: op={}", i, rule.op);
        data = apply_transform(rule, data, props);
        crate::vlo_debug!("🔧 After transform {}: {} items", i, data.len());
    }
    
    // Compute meta fields
    let mut meta = HashMap::new();
    for (name, rule) in &rules.meta {
        meta.insert(name.clone(), compute_meta_field(rule, &data));
    }
    
    // Apply computed fields from rules (percent_of_max, etc.)
    if !rules.computed.is_empty() {
        for item in data.iter_mut() {
            if let Some(obj) = item.as_object_mut() {
                for (name, rule) in &rules.computed {
                    let computed = compute_field_for_item(rule, obj, &meta);
                    obj.insert(name.clone(), computed);
                }
            }
        }
    }
    
    // Build config from props
    let mut config = props.clone();
    
    // Add chart-specific computed fields (percentage, x, y, path_data)
    add_computed_fields(&mut data, &mut meta, &mut config);
    
    crate::vlo_debug!("🔧 Final data: {} items", data.len());
    crate::vlo_debug!("🔧 Meta: {:?}", meta);
    
    Ok(HandlerOutput { data, meta, config })
}

fn load_handler_rules(module_path: &Path) -> Result<HandlerRules, String> {
    let manifest_path = module_path.join("module.json");
    let manifest_str = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Cannot read module.json: {}", e))?;
    
    let manifest: Value = serde_json::from_str(&manifest_str)
        .map_err(|e| format!("Invalid module.json: {}", e))?;
    
    if let Some(handler) = manifest.get("handler") {
        serde_json::from_value(handler.clone())
            .map_err(|e| format!("Invalid handler rules: {}", e))
    } else {
        Ok(HandlerRules::default())
    }
}

fn apply_transform(rule: &TransformRule, data: Vec<Value>, props: &HashMap<String, Value>) -> Vec<Value> {
    match rule.op.as_str() {
        "group" => {
            let key_field = resolve_template(rule.key.as_deref().unwrap_or(""), props);
            
            crate::vlo_debug!("🔧 Group transform: key_field='{}'", key_field);
            
            // Skip grouping if no key specified (data is pre-aggregated by SQL)
            if key_field.is_empty() || key_field == "{{group_by}}" {
                crate::vlo_debug!("🔧 Skipping group (empty key)");
                return data;
            }
            
            let value_field = resolve_template(rule.value.as_deref().unwrap_or("value"), props);
            let agg = rule.agg.as_deref().unwrap_or("sum");
            crate::vlo_debug!("🔧 Grouping by '{}' with value '{}'", key_field, value_field);
            group_by(&data, &key_field, &value_field, agg)
        }
        "sort" => {
            // Resolve template for sort key and order
            let key = resolve_template(rule.key.as_deref().unwrap_or(""), props);
            
            // Skip sorting if no key is provided (preserves SQL ORDER BY)
            if key.is_empty() {
                crate::vlo_debug!("🔧 Skipping sort (empty key)");
                return data;
            }
            
            let order = resolve_template(rule.order.as_deref().unwrap_or("asc"), props);
            sort_by(&data, &key, &order)
        }
        "limit" => {
            let count_str = resolve_template(rule.count.as_deref().unwrap_or("10"), props);
            let count = count_str.parse::<usize>().unwrap_or(10);
            data.into_iter().take(count).collect()
        }
        "filter" => {
            let field = rule.field.as_deref().unwrap_or("");
            let value = resolve_template(rule.value.as_deref().unwrap_or(""), props);
            filter_by(&data, field, &value)
        }
        "map" => {
            let from = resolve_template(rule.from.as_deref().unwrap_or(""), props);
            let to = resolve_template(rule.to.as_deref().unwrap_or(&from), props);
            crate::vlo_debug!("🔧 Map transform: '{}' -> '{}'", from, to);
            map_field(&data, &from, &to)
        }
        _ => data, // Unknown op = pass-through
    }
}

fn resolve_template(template: &str, props: &HashMap<String, Value>) -> String {
    let mut result = template.to_string();
    for (key, value) in props {
        let placeholder = format!("{{{{{}}}}}", key);
        if result.contains(&placeholder) {
            let val_str = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => value.to_string(),
            };
            result = result.replace(&placeholder, &val_str);
        }
    }
    result
}

fn group_by(data: &[Value], key_field: &str, value_field: &str, agg: &str) -> Vec<Value> {
    let mut groups: HashMap<String, Vec<f64>> = HashMap::new();
    
    for row in data {
        if let Some(obj) = row.as_object() {
            let key = obj.get(key_field)
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "Other".to_string());
            
            let value = obj.get(value_field)
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            
            groups.entry(key).or_default().push(value);
        }
    }
    
    let mut result: Vec<Value> = groups.into_iter()
        .map(|(label, values)| {
            let aggregated = match agg {
                "avg" => values.iter().sum::<f64>() / values.len().max(1) as f64,
                "count" => values.len() as f64,
                "min" => values.iter().cloned().fold(f64::INFINITY, f64::min),
                "max" => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                _ => values.iter().sum::<f64>(),
            };
            serde_json::json!({
                "label": label,
                "value": aggregated,
                "count": values.len()
            })
        })
        .collect();
    
    result.sort_by(|a, b| {
        let va = a.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let vb = b.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    
    result
}

fn sort_by(data: &[Value], key: &str, order: &str) -> Vec<Value> {
    let mut result = data.to_vec();
    result.sort_by(|a, b| {
        let va = get_nested(a, key);
        let vb = get_nested(b, key);
        let cmp = compare_values(&va, &vb);
        if order == "desc" { cmp.reverse() } else { cmp }
    });
    result
}

fn filter_by(data: &[Value], field: &str, value: &str) -> Vec<Value> {
    data.iter()
        .filter(|row| {
            if let Some(obj) = row.as_object() {
                if let Some(v) = obj.get(field) {
                    let v_str = match v {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        Value::Bool(b) => b.to_string(),
                        _ => v.to_string(),
                    };
                    return v_str == value;
                }
            }
            false
        })
        .cloned()
        .collect()
}

fn map_field(data: &[Value], from: &str, to: &str) -> Vec<Value> {
    data.iter()
        .map(|row| {
            let mut new_row = row.clone();
            if let Some(obj) = new_row.as_object_mut() {
                let value = obj.get(from).cloned().unwrap_or(Value::Null);
                
                // Handle null/empty values as "Other"
                let final_value = if value.is_null() 
                    || (value.is_string() && value.as_str().unwrap_or("").trim().is_empty()) {
                    Value::String("Other".to_string())
                } else {
                    value
                };
                
                obj.insert(to.to_string(), final_value);
            }
            new_row
        })
        .collect()
}

fn compute_meta_field(rule: &MetaRule, data: &[Value]) -> Value {
    let values: Vec<f64> = data.iter()
        .filter_map(|row| {
            row.as_object()
                .and_then(|obj| obj.get(&rule.field))
                .and_then(|v| v.as_f64())
        })
        .collect();
    
    match rule.op.as_str() {
        "sum" => serde_json::json!(values.iter().sum::<f64>()),
        "avg" => {
            if values.is_empty() { serde_json::json!(0.0) }
            else { serde_json::json!(values.iter().sum::<f64>() / values.len() as f64) }
        }
        "min" => serde_json::json!(values.iter().cloned().fold(f64::INFINITY, f64::min)),
        "max" => serde_json::json!(values.iter().cloned().fold(f64::NEG_INFINITY, f64::max)),
        "count" => serde_json::json!(values.len()),
        _ => serde_json::json!(null),
    }
}

fn compute_field_for_item(rule: &ComputedRule, item: &serde_json::Map<String, Value>, meta: &HashMap<String, Value>) -> Value {
    match rule.op.as_str() {
        "percent_of_max" => {
            let value = item.get(&rule.field).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let max = meta.get("max_value")
                .and_then(|v| v.as_f64())
                .or_else(|| meta.values().find_map(|v| v.as_f64()))
                .unwrap_or(1.0);
            let pct = if max > 0.0 { (value / max * 100.0).round() } else { 0.0 };
            serde_json::json!(pct)
        }
        "percent_of_total" => {
            let value = item.get(&rule.field).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let total = meta.get("total").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let pct = if total > 0.0 { (value / total * 100.0).round() } else { 0.0 };
            serde_json::json!(pct)
        }
        _ => serde_json::json!(null),
    }
}

/// Add chart-specific computed fields (percentage, SVG coordinates, path_data)
fn add_computed_fields(
    data: &mut Vec<Value>, 
    _meta: &mut HashMap<String, Value>,
    config: &mut HashMap<String, Value>
) {
    if data.is_empty() {
        return;
    }
    
    // Find max value for percentage calculations
    let max = data.iter()
        .filter_map(|item| {
            item.as_object()
                .and_then(|obj| obj.get("value"))
                .and_then(|v| v.as_f64())
        })
        .fold(0.0_f64, f64::max);
    
    let len = data.len();
    let mut path_points = Vec::new();
    
    for (i, item) in data.iter_mut().enumerate() {
        if let Some(obj) = item.as_object_mut() {
            let value = obj.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
            
            // 1. Bar chart percentage
            let percentage = if max > 0.0 { (value / max * 100.0).round() } else { 0.0 };
            obj.insert("percentage".to_string(), serde_json::json!(percentage));
            
            // 2. Line chart SVG coordinates
            // X: evenly spaced across 380px (with 10px padding on each side)
            let x = if len > 1 { 
                (i as f64 / (len - 1) as f64) * 380.0 + 10.0 
            } else { 
                200.0 
            };
            
            // Y: inverted (190 is bottom, 10 is top)
            let y = if max > 0.0 { 
                190.0 - (value / max * 180.0) 
            } else { 
                190.0 
            };
            
            obj.insert("x".to_string(), serde_json::json!(x.round()));
            obj.insert("y".to_string(), serde_json::json!(y.round()));
            
            path_points.push(format!("{} {}", x.round(), y.round()));
        }
    }
    
    // 3. Build SVG path string (e.g., "M 10 150 L 100 50 L 200 100")
    if !path_points.is_empty() {
        let path_data = if path_points.len() == 1 {
            format!("M {}", path_points[0])
        } else {
            format!("M {} L {}", path_points[0], path_points[1..].join(" L "))
        };
        config.insert("path_data".to_string(), serde_json::json!(path_data));
    }
}

fn get_nested(value: &Value, path: &str) -> Value {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value.clone();
    for part in parts {
        match current {
            Value::Object(map) => {
                current = map.get(part).cloned().unwrap_or(Value::Null);
            }
            _ => return Value::Null,
        }
    }
    current
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            a.as_f64().unwrap_or(0.0).partial_cmp(&b.as_f64().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    }
}