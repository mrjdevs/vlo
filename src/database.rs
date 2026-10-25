use crate::features::{Feature, is_enabled};
use crate::state::get_project_root;
use serde_json::Value;
use sqlx::{Column, Row, ValueRef, TypeInfo};
use std::{fs, path::Path, sync::{LazyLock, OnceLock}};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DbColumnType {
    Integer,
    Float,
    Boolean,
    Text,
    DateTime,
    Json,
    Blob,
    Unknown,
}

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
// 3. ROW TO JSON MACROS (Type-hint optimized & edge-case hardened)
// ===========================================================================

/// Classifies the columns ONCE per query, returning a Vec of parsed types.
#[macro_export]
macro_rules! get_column_types {
    ($row:expr) => {{
        let mut types = Vec::with_capacity($row.columns().len());

        for column in $row.columns() {
            let type_name = column.type_info().name().to_lowercase();
            let col_type = if type_name.contains("int")
                || type_name.contains("serial")
                || type_name.contains("int2")
                || type_name.contains("int4")
                || type_name.contains("int8")
                || type_name.contains("bigint")
                || type_name.contains("smallint")
                || type_name.contains("tinyint")
                || type_name.contains("mediumint")
                || type_name.contains("unsigned")
                || type_name.contains("year")
            {
                $crate::database::DbColumnType::Integer
            } else if type_name.contains("float")
                || type_name.contains("double")
                || type_name.contains("real")
                || type_name.contains("numeric")
                || type_name.contains("decimal")
            {
                $crate::database::DbColumnType::Float
            } else if type_name.contains("bool") || type_name == "bit" {
                $crate::database::DbColumnType::Boolean
            } else if type_name.contains("datetime")
                || type_name.contains("timestamp")
                || type_name == "date"
                || type_name.contains("time")
            {
                $crate::database::DbColumnType::DateTime
            } else if type_name.contains("json") {
                $crate::database::DbColumnType::Json
            } else if type_name.contains("blob")
                || type_name.contains("bytea")
                || type_name.contains("binary")
                || type_name.contains("varbinary")
                || type_name.contains("bit varying")
            {
                $crate::database::DbColumnType::Blob
            } else if type_name.contains("text")
                || type_name.contains("char")
                || type_name.contains("varchar")
                || type_name.contains("uuid")
                || type_name.contains("enum")
                || type_name.contains("clob")
            {
                $crate::database::DbColumnType::Text
            } else {
                $crate::database::DbColumnType::Unknown
            };

            types.push(col_type);
        }

        types
    }};
}
/// Converts a row to JSON using the pre-classified column types.

#[macro_export]
macro_rules! convert_row_to_json {
    ($row:expr, $col_types:expr) => {{
        let columns = $row.columns();
        let col_types = &$col_types;
        let mut map = serde_json::Map::with_capacity(columns.len());

        for (i, column) in columns.iter().enumerate() {
            let name = column.name().to_string();
            let is_null = $row.try_get_raw(i).map(|r| r.is_null()).unwrap_or(true);

            if is_null {
                map.insert(name, serde_json::Value::Null);
                continue;
            }

            let val = match col_types[i] {
                $crate::database::DbColumnType::Integer => {
                    if let Ok(v) = $row.try_get::<i64, _>(i) {
                        serde_json::Value::from(v)
                    } else if let Ok(v) = $row.try_get::<i32, _>(i) {
                        serde_json::Value::from(v)
                    } else if let Ok(v) = $row.try_get::<i16, _>(i) {
                        serde_json::Value::from(v)
                    } else if let Ok(v) = $row.try_get::<String, _>(i) {
                        v.parse::<i64>()
                            .map(serde_json::Value::from)
                            .unwrap_or(serde_json::Value::Null)
                    } else {
                        serde_json::Value::Null
                    }
                }
                $crate::database::DbColumnType::Float => {
                    if let Ok(v) = $row.try_get::<f64, _>(i) {
                        serde_json::Number::from_f64(v)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    } else if let Ok(v) = $row.try_get::<f32, _>(i) {
                        serde_json::Number::from_f64(v as f64)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    } else if let Ok(v) = $row.try_get::<String, _>(i) {
                        v.parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    } else {
                        serde_json::Value::Null
                    }
                }
                $crate::database::DbColumnType::Boolean => {
                    if let Ok(v) = $row.try_get::<bool, _>(i) {
                        serde_json::Value::Bool(v)
                    } else if let Ok(v) = $row.try_get::<i64, _>(i) {
                        serde_json::Value::Bool(v != 0)
                    } else if let Ok(v) = $row.try_get::<i32, _>(i) {
                        serde_json::Value::Bool(v != 0)
                    } else {
                        serde_json::Value::Null
                    }
                }
                $crate::database::DbColumnType::DateTime => {
                    if let Ok(v) = $row.try_get::<String, _>(i) {
                        serde_json::Value::String(v)
                    } else {
                        serde_json::Value::Null
                    }
                }
                $crate::database::DbColumnType::Json => {
                    // 1. Try native JSON (Postgres JSONB)
                    if let Ok(v) = $row.try_get::<serde_json::Value, _>(i) {
                        v
                    } 
                    // 2. Fallback to String (SQLite/MySQL JSON_EXTRACT)
                    else if let Ok(s) = $row.try_get::<String, _>(i) {
                        // If it's a valid JSON string, parse it. If it's just a raw string, keep it.
                        serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
                    } 
                    // 3. Last resort (numbers, floats, etc)
                    else if let Ok(v) = $row.try_get::<i64, _>(i) {
                        serde_json::Value::from(v)
                    } else if let Ok(v) = $row.try_get::<f64, _>(i) {
                        serde_json::Number::from_f64(v).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null)
                    } else {
                        serde_json::Value::Null
                    }
                }
                $crate::database::DbColumnType::Blob => {
                    $row.try_get::<Vec<u8>, _>(i)
                        .map(|v| serde_json::Value::String(format!("blob {}b", v.len())))
                        .unwrap_or(serde_json::Value::Null)
                }
                _ => {
                    if let Ok(v) = $row.try_get::<String, _>(i) {
                        serde_json::Value::String(v)
                    } else if let Ok(v) = $row.try_get::<i64, _>(i) {
                        serde_json::Value::from(v)
                    } else if let Ok(v) = $row.try_get::<f64, _>(i) {
                        serde_json::Number::from_f64(v)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    } else if let Ok(v) = $row.try_get::<bool, _>(i) {
                        serde_json::Value::Bool(v)
                    } else {
                        serde_json::Value::Null
                    }
                }
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
                let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&abs).create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal).synchronous(sqlx::sqlite::SqliteSynchronous::Normal).busy_timeout(std::time::Duration::from_secs(5));
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
            
            // Extract types ONCE for the entire result set
            let col_types = if let Some(first_row) = rows.first() {
                get_column_types!(first_row)
            } else {
                return Ok(vec![]); // No rows, no work to do
            };

            // Convert rows using the pre-calculated types
            let json_rows = rows.iter().map(|row| convert_row_to_json!(row, &col_types)).collect();
            Ok(json_rows)
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

    /// SQL to check if a table exists. Returns rows if yes, empty if no.
    pub fn table_exists_sql(&self, table_name: &str) -> String {
        let safe = table_name.replace('\'', "''");
        match self {
            DbPool::Sqlite(_) => format!(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='{}'",
                safe
            ),
            DbPool::Postgres(_) => format!(
                "SELECT table_name FROM information_schema.tables WHERE table_schema='public' AND table_name='{}'",
                safe
            ),
            DbPool::MySql(_) => format!(
                "SELECT table_name FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='{}'",
                safe
            ),
        }
    }

    /// SQL to fetch the columns of a table. Returns one row per column.
    pub fn table_columns_sql(&self, table_name: &str) -> String {
        let safe = table_name.replace('\'', "''");
        match self {
            DbPool::Sqlite(_) => format!("PRAGMA table_info({})", safe),
            DbPool::Postgres(_) => format!(
                "SELECT column_name FROM information_schema.columns WHERE table_name='{}' AND table_schema='public'",
                safe
            ),
            DbPool::MySql(_) => format!(
                "SELECT column_name FROM information_schema.columns WHERE table_schema=DATABASE() AND table_name='{}'",
                safe
            ),
        }
    }

    /// Which key in the result of `table_columns_sql` holds the column name?
    pub fn table_columns_key(&self) -> &'static str {
        match self {
            DbPool::Sqlite(_) => "name",
            DbPool::Postgres(_) | DbPool::MySql(_) => "column_name",
        }
    }

    /// DB-specific suffix for an auto-incrementing primary key.
    pub fn primary_key_suffix(&self) -> &'static str {
        match self {
            DbPool::Sqlite(_) => " PRIMARY KEY AUTOINCREMENT",
            DbPool::Postgres(_) => " SERIAL PRIMARY KEY",
            DbPool::MySql(_) => " AUTO_INCREMENT PRIMARY KEY",
        }
    }

    /// Translates VLO standard SQL (SQLite syntax) to the target database dialect.
    pub fn translate_sql(&self, sql: &str) -> String {
        match self {
            DbPool::Sqlite(_) => sql.to_string(),
            
            DbPool::Postgres(_) => {
                let mut out = sql.to_string();
                out = out.replace("json_object(", "jsonb_build_object(");
                out = out.replace("json_extract(", "jsonb_extract_path_text(");
                out
            }
            
            DbPool::MySql(_) => {
                static CONCAT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"'([^']*)'\s*\|\|\s*([a-zA-Z0-9_.\{\}]+)\s*\|\|\s*'([^']*)'").unwrap()
                });
                
                let mut out = sql.to_string();
                out = out.replace("json_object(", "JSON_OBJECT(");
                out = out.replace("json_extract(", "JSON_EXTRACT(");
                out = CONCAT_RE.replace_all(&out, "CONCAT('$1', $2, '$3')").into_owned();
                out
            }
        }
    }

    /// Execute multiple statements atomically in a transaction.
    pub async fn execute_multi_statement(&self, statements: &[String]) -> Result<Vec<Value>, String> {
        match self {
            DbPool::Sqlite(c) => run_tx_sqlite(c, statements).await,
            DbPool::Postgres(c) => run_tx_pg(c, statements).await,
            DbPool::MySql(c) => run_tx_mysql(c, statements).await,
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

macro_rules! impl_transaction {
    ($fn_name:ident, $pool_type:ty) => {
        async fn $fn_name(pool: &$pool_type, stmts: &[String]) -> Result<Vec<Value>, String> {
            let mut tx = pool.begin().await.map_err(|e| format!("Begin tx: {}", e))?;
            let mut last_rows = Vec::new();
            let total = stmts.len();

            for (idx, s) in stmts.iter().enumerate() {
                match sqlx::query(s).fetch_all(&mut *tx).await {
                    Ok(rows) => {
                        let col_types = if let Some(first_row) = rows.first() {
                            get_column_types!(first_row)
                        } else {
                            vec![]
                        };
                        last_rows = rows.iter().map(|row| convert_row_to_json!(row, &col_types)).collect();
                    }
                    Err(e) => {
                        if let Err(rb) = tx.rollback().await {
                            crate::vlo_debug!("⚠️ [TX] Rollback failed after stmt {}: {}", idx + 1, rb);
                        }
                        let preview = if s.len() > 300 {
                            format!("{}… ({} chars)", &s[..300], s.len())
                        } else {
                            s.to_string()
                        };
                        crate::vlo_debug!("❌ [TX] Statement {}/{} FAILED", idx + 1, total);
                        crate::vlo_debug!("❌ [TX] SQL: {}", preview);
                        crate::vlo_debug!("❌ [TX] Error: {}", e);
                        return Err(format!(
                            "SQL error at statement {}/{}: {}\n--- SQL ---\n{}",
                            idx + 1, total, e, preview
                        ));
                    }
                }
            }
            tx.commit().await.map_err(|e| format!("Commit tx: {}", e))?;
            Ok(last_rows)
        }
    };
}

impl_transaction!(run_tx_sqlite, sqlx::SqlitePool);
impl_transaction!(run_tx_pg, sqlx::PgPool);
impl_transaction!(run_tx_mysql, sqlx::MySqlPool);
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