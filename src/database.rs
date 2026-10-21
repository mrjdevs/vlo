use crate::features::{Feature, is_enabled};
use crate::state::get_project_root;
use serde_json::Value;
use sqlx::{Column, Row, ValueRef, TypeInfo};
use std::{fs, path::Path, sync::OnceLock};

// ===========================================================================
// 1. DB POOL & SINGLE SOURCE OF TRUTH
// ===========================================================================
#[derive(Clone)]
pub enum DbPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
}

pub static DB_POOL: OnceLock<DbPool> = OnceLock::new();

#[macro_export]
macro_rules! for_each_db {
    ($pool:expr, |$conn:ident| $body:expr) => {
        match $pool {
            $crate::database::DbPool::Sqlite($conn) => $body,
            $crate::database::DbPool::Postgres($conn) => $body,
            $crate::database::DbPool::MySql($conn) => $body,
        }
    };
}

// ===========================================================================
// 2. SAFE ACCESS & DRIVER RESOLUTION
// ===========================================================================
pub fn get_pool() -> Result<&'static DbPool, String> {
    if !is_enabled(Feature::Database) { return Err("Database feature is disabled".into()); }
    DB_POOL.get().ok_or_else(|| "Database pool not initialized. Check DATABASE_URL.".into())
}

fn resolve_driver(url: &str) -> String {
    if let Ok(d) = std::env::var("DB_DRIVER") { let t = d.trim().to_lowercase(); if !t.is_empty() { return t; } }
    let lower = url.trim().to_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") { return "postgres".into(); }
    if lower.starts_with("mysql://") { return "mysql".into(); }
    "sqlite".into()
}

// ===========================================================================
// 3. ROW TO JSON MACRO (Type-hint optimized & edge-case hardened)
// ===========================================================================
macro_rules! convert_row_to_json {
    ($row:expr) => {{
        
        let columns = $row.columns();
        let mut map = serde_json::Map::with_capacity(columns.len());
        
        for (i, column) in columns.iter().enumerate() {
            let name = column.name().to_string();
            
            // Fast path: check if null first
            let is_null = $row.try_get_raw(i).map(|r| r.is_null()).unwrap_or(true);
            if is_null {
                map.insert(name, serde_json::Value::Null);
                continue;
            }
            
            // Get type hint from column metadata to avoid blind try_get chain
            let type_name = column.type_info().name();
            let type_lower = type_name.to_lowercase();
            
            let val: serde_json::Value = if type_lower.contains("int") || type_lower.contains("serial") {
                // Integer types (INT, INTEGER, BIGINT, SERIAL, etc.)
                if let Ok(v) = $row.try_get::<i64, _>(i) {
                    serde_json::Value::Number(v.into())
                } else if let Ok(v) = $row.try_get::<i32, _>(i) {
                    serde_json::Value::Number((v as i64).into())
                } else {
                    serde_json::Value::Null
                }
            } else if type_lower.contains("timestamp") || type_lower.contains("date") || type_lower.contains("time") {
                // 🚀 OPTIMIZATION: Timestamps/Dates. 
                // sqlx returns these as formatted strings when cast to String.
                // This avoids the 4 failing numeric/bool attempts in the fallback.
                if let Ok(v) = $row.try_get::<String, _>(i) {
                    serde_json::Value::String(v)
                } else {
                    serde_json::Value::Null
                }
            } else if type_lower.contains("float") || type_lower.contains("double") || type_lower.contains("real") {
                // Float types (FLOAT, DOUBLE, REAL)
                if let Ok(v) = $row.try_get::<f64, _>(i) {
                    serde_json::Number::from_f64(v).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null)
                } else {
                    serde_json::Value::Null
                }
            } else if type_lower.contains("numeric") || type_lower.contains("decimal") {
                // 🚀 OPTIMIZATION: Safer NUMERIC/DECIMAL handling.
                // Postgres NUMERIC may not decode directly to f64 without specific sqlx features.
                // Try i64 first (for integer-like numerics), then fall back to String parsing.
                if let Ok(v) = $row.try_get::<i64, _>(i) {
                    serde_json::Value::Number(v.into())
                } else if let Ok(v) = $row.try_get::<String, _>(i) {
                    if let Ok(n) = v.parse::<i64>() {
                        serde_json::Value::Number(n.into())
                    } else if let Ok(n) = v.parse::<f64>() {
                        serde_json::Number::from_f64(n).map(serde_json::Value::Number).unwrap_or(serde_json::Value::String(v))
                    } else {
                        serde_json::Value::String(v)
                    }
                } else {
                    serde_json::Value::Null
                }
            } else if type_lower.contains("bool") {
                // Boolean types (BOOL, BOOLEAN)
                if let Ok(v) = $row.try_get::<bool, _>(i) {
                    serde_json::Value::Bool(v)
                } else {
                    serde_json::Value::Null
                }
            } else if type_lower.contains("text") || type_lower.contains("char") || type_lower.contains("varchar") || type_lower.contains("string") || type_lower.contains("clob") || type_lower.contains("json") || type_lower.contains("uuid") {
                // String-like types (TEXT, VARCHAR, CHAR, JSON, UUID, etc.)
                if let Ok(v) = $row.try_get::<String, _>(i) {
                    // Some DBs return numbers as strings, try to parse
                    if let Ok(n) = v.parse::<i64>() {
                        serde_json::Value::Number(n.into())
                    } else if let Ok(n) = v.parse::<f64>() {
                        serde_json::Number::from_f64(n).map(serde_json::Value::Number).unwrap_or(serde_json::Value::String(v))
                    } else {
                        serde_json::Value::String(v)
                    }
                } else {
                    serde_json::Value::Null
                }
            } else if type_lower.contains("blob") || type_lower.contains("bytea") || type_lower.contains("binary") {
                // Binary types (BLOB, BYTEA, BINARY)
                if let Ok(v) = $row.try_get::<Vec<u8>, _>(i) {
                    serde_json::Value::String(format!("blob {}b", v.len()))
                } else {
                    serde_json::Value::Null
                }
            } else {
                // Fallback: try the original chain for unknown/custom types (e.g., PostGIS POINT)
                if let Ok(v) = $row.try_get::<i64, _>(i) { serde_json::Value::Number(v.into()) }
                else if let Ok(v) = $row.try_get::<i32, _>(i) { serde_json::Value::Number((v as i64).into()) }
                else if let Ok(v) = $row.try_get::<f64, _>(i) { serde_json::Number::from_f64(v).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null) }
                else if let Ok(v) = $row.try_get::<bool, _>(i) { serde_json::Value::Bool(v) }
                else if let Ok(v) = $row.try_get::<String, _>(i) {
                    if let Ok(n) = v.parse::<i64>() { serde_json::Value::Number(n.into()) }
                    else if let Ok(n) = v.parse::<f64>() { serde_json::Number::from_f64(n).map(serde_json::Value::Number).unwrap_or(serde_json::Value::String(v)) }
                    else { serde_json::Value::String(v) }
                }
                else if let Ok(v) = $row.try_get::<Vec<u8>, _>(i) { serde_json::Value::String(format!("blob {}b", v.len())) }
                else { serde_json::Value::Null }
            };
            
            map.insert(name, val);
        }
        serde_json::Value::Object(map)
    }};
}

// ===========================================================================
// 4. DbPool IMPLEMENTATION
// ===========================================================================
impl DbPool {
    pub async fn connect(driver: &str, url: &str, root: &Path) -> Result<Self, String> {
        match driver {
            "postgres" | "postgresql" => sqlx::PgPool::connect(url).await.map(DbPool::Postgres).map_err(|e| format!("PostgreSQL connection failed: {e}")),
            "mysql" => sqlx::MySqlPool::connect(url).await.map(DbPool::MySql).map_err(|e| format!("MySQL connection failed: {e}")),
            _ => {
                let path = url.strip_prefix("sqlite://").or_else(|| url.strip_prefix("sqlite:")).unwrap_or(url);
                let abs = if Path::new(path).is_absolute() { Path::new(path).to_path_buf() } else { root.join(path) };
                let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&abs).create_if_missing(true);
                sqlx::SqlitePool::connect_with(opts).await.map(DbPool::Sqlite).map_err(|e| format!("SQLite connection failed: {e}"))
            }
        }
    }

    pub async fn execute(&self, sql: &str) -> Result<u64, sqlx::Error> {
        for_each_db!(self, |c| sqlx::query(sql).execute(c).await.map(|r| r.rows_affected()))
    }

    pub async fn fetch_all_json(&self, sql: &str) -> Result<Vec<Value>, sqlx::Error> {
        for_each_db!(self, |c| {
            let rows = sqlx::query(sql).fetch_all(c).await?;
            Ok(rows.iter().map(|row| convert_row_to_json!(row)).collect())
        })
    }

    pub fn placeholder(&self, n: usize) -> String {
        match self { DbPool::Postgres(_) => format!("${n}"), _ => "?".into() }
    }

    pub fn date_add_seconds_sql(&self, param_idx: usize) -> String {
        let p = self.placeholder(param_idx);
        match self {
            DbPool::Postgres(_) => format!("CURRENT_TIMESTAMP + ({} || ' seconds')::interval", p),
            DbPool::MySql(_) => format!("DATE_ADD(NOW(), INTERVAL {} SECOND)", p),
            DbPool::Sqlite(_) => format!("datetime('now', '+' || {} || ' seconds')", p),
        }
    }
}

// ===========================================================================
// 5. CROSS-DB MACROS
// ===========================================================================
#[macro_export]
macro_rules! db_execute {
    ($pool:expr, $sql:expr $(, $bind:expr)*) => {
        $crate::for_each_db!($pool, |c| ::sqlx::query($sql) $(.bind($bind))*.execute(c).await.map(|r| r.rows_affected()))
    };
}

#[macro_export]
macro_rules! db_fetch_optional_as {
    ($pool:expr, $type:ty, $sql:expr $(, $bind:expr)*) => {
        $crate::for_each_db!($pool, |c| ::sqlx::query_as::<_, $type>($sql) $(.bind($bind))*.fetch_optional(c).await)
    };
}

#[macro_export]
macro_rules! db_fetch_optional_scalar_string {
    ($pool:expr, $sql:expr $(, $bind:expr)*) => {
        $crate::for_each_db!($pool, |c| ::sqlx::query_scalar::<_, String>($sql) $(.bind($bind))*.fetch_optional(c).await)
    };
}

#[macro_export]
macro_rules! db_fetch_optional_scalar_i64 {
    ($pool:expr, $sql:expr $(, $bind:expr)*) => {
        $crate::for_each_db!($pool, |c| ::sqlx::query_scalar::<_, i64>($sql) $(.bind($bind))*.fetch_optional(c).await)
    };
}

#[macro_export]
macro_rules! db_fetch_one_scalar_i64 {
    ($pool:expr, $sql:expr $(, $bind:expr)*) => {
        $crate::for_each_db!($pool, |c| ::sqlx::query_scalar::<_, i64>($sql) $(.bind($bind))*.fetch_one(c).await)
    };
}

// ===========================================================================
// 6. INITIALIZATION
// ===========================================================================
pub async fn init_db() -> Result<(), String> {
    let root = get_project_root();
    let db_url = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => { eprintln!("⚠️ DATABASE_URL not set. DB features disabled."); return Ok(()); }
    };

    let driver = resolve_driver(&db_url);
    crate::vlo_debug!("🔧 Database driver resolved to '{}'", driver);
    let pool = DbPool::connect(&driver, &db_url, &root).await?;

    let schema_file = std::env::var("DB_SCHEMA").unwrap_or_else(|_| "schema.sql".into());
    let schema_path = root.join(&schema_file);

    if schema_path.exists() {
        let sql = fs::read_to_string(&schema_path).map_err(|e| format!("Failed to read {}: {}", schema_file, e))?;
        let mut count = 0;
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty() && !s.to_uppercase().starts_with("INSERT")) {
            count += 1;
            if let Err(err) = pool.execute(stmt).await { crate::vlo_debug!("⚠️ Schema statement failed: {}", err); }
        }
        crate::vlo_debug!("✅ {} processed ({} statements)", schema_file, count);
    }

    if DB_POOL.set(pool).is_ok() { crate::vlo_debug!("✅ Database pool initialized"); }
    Ok(())
}