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
                    } else if let Ok(v) = $row.try_get::<chrono::NaiveDateTime, _>(i) {
                        serde_json::Value::String(v.format("%Y-%m-%d %H:%M:%S").to_string())
                    } else if let Ok(v) = $row.try_get::<chrono::DateTime<chrono::Utc>, _>(i) {
                        serde_json::Value::String(v.format("%Y-%m-%d %H:%M:%S").to_string())
                    } else if let Ok(v) = $row.try_get::<chrono::NaiveDate, _>(i) {
                        serde_json::Value::String(v.format("%Y-%m-%d").to_string())
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

    /// Translates VLO standard SQL (SQLite syntax) to the target database dialect.
    pub fn translate_sql(&self, sql: &str) -> String {
        match self {
            DbPool::Sqlite(_) => sql.to_string(),
            
            DbPool::Postgres(_) => {
                static JSON_ARRAY_PATH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"json_extract\(\s*([A-Za-z_][A-Za-z0-9_.]*)\s*,\s*'\$\[(\d+)\]'\s*\)").unwrap()
                });
                static JSON_KEY_PATH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"json_extract\(\s*([A-Za-z_][A-Za-z0-9_.]*)\s*,\s*'\$\.([A-Za-z_][A-Za-z0-9_]*)'\s*\)").unwrap()
                });
                static JSON_VALID_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"json_valid\(\s*([A-Za-z_][A-Za-z0-9_.]*)\s*\)").unwrap()
                });
                static STRFTIME_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"strftime\(\s*'([^']*)'\s*,\s*([^()]+?)\s*\)").unwrap()
                });

                let mut out = sql.to_string();
                out = JSON_ARRAY_PATH_RE.replace_all(&out, "($1::jsonb ->> $2)").into_owned();
                out = JSON_KEY_PATH_RE.replace_all(&out, "($1::jsonb ->> '$2')").into_owned();
                out = JSON_VALID_RE.replace_all(&out, "($1 IS NOT NULL AND LEFT($1::text, 1) IN ('[', '{'))").into_owned();
                out = STRFTIME_RE.replace_all(&out, |caps: &regex::Captures| {
                    let arg = caps[2].trim();
                    let arg = if arg.eq_ignore_ascii_case("'now'") { "CURRENT_TIMESTAMP" } else { arg };
                    format!("to_char({}, '{}')", arg, pg_date_format(&caps[1]))
                }).into_owned();
                out = out.replace("json_object(", "jsonb_build_object(");
                out = pg_json_object_to_text(&out);
                out = out.replace("json_extract(", "jsonb_extract_path_text(");
                out = pg_nullif_empty_to_text(&out);
                out
            }
            
            DbPool::MySql(_) => {
                static JSON_EXTRACT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"JSON_EXTRACT\(\s*([A-Za-z_][A-Za-z0-9_.]*)\s*,\s*'(\$[^']*)'\s*\)").unwrap()
                });
                static STRFTIME_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"strftime\(\s*'([^']*)'\s*,\s*([^()]+?)\s*\)").unwrap()
                });
                static ON_CONFLICT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"(?i)ON\s+CONFLICT\s*\([^)]*\)\s*DO\s+UPDATE\s+SET\s+").unwrap()
                });
                static EXCLUDED_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"(?i)\bexcluded\.([A-Za-z_][A-Za-z0-9_]*)").unwrap()
                });

                let mut out = sql.to_string();
                out = out.replace("json_object(", "JSON_OBJECT(");
                out = out.replace("json_extract(", "JSON_EXTRACT(");
                out = JSON_EXTRACT_RE.replace_all(&out, "JSON_UNQUOTE(JSON_EXTRACT($1, '$2'))").into_owned();
                out = STRFTIME_RE.replace_all(&out, |caps: &regex::Captures| {
                    let arg = caps[2].trim();
                    let arg = if arg.eq_ignore_ascii_case("'now'") { "NOW()".to_string() } else { arg.to_string() };
                    format!("DATE_FORMAT({}, '{}')", arg, mysql_date_format(&caps[1]))
                }).into_owned();
                out = mysql_concat(&out);
                out = out.replace("AS INTEGER", "AS SIGNED");
                out = ON_CONFLICT_RE.replace_all(&out, "ON DUPLICATE KEY UPDATE ").into_owned();
                out = EXCLUDED_RE.replace_all(&out, "VALUES($1)").into_owned();
                out
            }
        }
    }

    /// Adapts one statement from a SQLite-flavored schema file to the active
    /// dialect. Returns `None` for statements that have no meaning there.
    pub fn prepare_schema_statement(&self, sql: &str) -> Option<String> {
        translate_schema_statement(self.driver_name(), sql)
    }

    /// The driver name behind this pool: "sqlite" | "postgres" | "mysql".
    pub fn driver_name(&self) -> &'static str {
        match self {
            DbPool::Sqlite(_) => "sqlite",
            DbPool::Postgres(_) => "postgres",
            DbPool::MySql(_) => "mysql",
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

/// Adapts one statement from the SQLite-flavored `schema.sql` to `driver`.
/// This is the single place new database types are taught about our DDL.
pub fn translate_schema_statement(driver: &str, sql: &str) -> Option<String> {
    let code = sql
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let code = code.trim();
    if code.is_empty() {
        return None;
    }

    let upper = code.to_uppercase();
    if driver == "sqlite" {
        return Some(code.to_string());
    }
    if upper.starts_with("PRAGMA") {
        return None;
    }

    match driver {
        "postgres" => {
            let mut s = code
                .replace("INTEGER PRIMARY KEY AUTOINCREMENT", "BIGSERIAL PRIMARY KEY")
                .replace("DATETIME", "TIMESTAMP")
                .replace(" REAL ", " DOUBLE PRECISION ")
                .replace(" NUMERIC ", " DOUBLE PRECISION ")
                .replace(
                    "TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
                    "TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP",
                );
            if s.to_uppercase().starts_with("INSERT OR IGNORE INTO") {
                s = s.replacen("INSERT OR IGNORE INTO", "INSERT INTO", 1);
                s.push_str(" ON CONFLICT DO NOTHING");
            }
            Some(s)
        }
        "mysql" => {
            static TEXT_TS_DEFAULT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                regex::Regex::new(r"(?i)\bTEXT\s+NOT\s+NULL\s+DEFAULT\s+CURRENT_TIMESTAMP\b").unwrap()
            });
            static TEXT_LITERAL_DEFAULT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                regex::Regex::new(r"(?i)\bTEXT(\s+(?:NOT\s+NULL\s+)?DEFAULT\s+'[^']*')").unwrap()
            });

            // MySQL cannot index or default a bare TEXT column, and foreign
            // keys must match the BIGINT primary keys one-for-one.
            let mut s = code.replace("INTEGER PRIMARY KEY AUTOINCREMENT", "BIGINT AUTO_INCREMENT PRIMARY KEY");
            s = s.replace("TEXT CHECK", "VARCHAR(255) CHECK");
            s = TEXT_TS_DEFAULT_RE
                .replace_all(&s, "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP")
                .into_owned();
            s = TEXT_LITERAL_DEFAULT_RE.replace_all(&s, "VARCHAR(255)$1").into_owned();
            s = s.replace("TEXT UNIQUE NOT NULL", "VARCHAR(255) UNIQUE NOT NULL");
            s = s.replace("TEXT NOT NULL UNIQUE", "VARCHAR(255) NOT NULL UNIQUE");
            s = s.replace("TEXT PRIMARY KEY", "VARCHAR(255) PRIMARY KEY");
            // Session ids are short and indexed; MySQL cannot index bare TEXT.
            s = s.replace("session_id TEXT", "session_id VARCHAR(255)");
            s = s.replace("INTEGER", "BIGINT");
            s = s.replace(" REAL ", " DOUBLE ").replace(" NUMERIC ", " DOUBLE ");
            s = s.replace("CREATE INDEX IF NOT EXISTS", "CREATE INDEX");
            s = s.replace("INSERT OR IGNORE INTO", "INSERT IGNORE INTO");
            Some(s)
        }
        _ => Some(code.to_string()),
    }
}

/// Emits the project `schema.sql` translated for `driver`, including the seed
/// rows. Used by `vlo schema --driver <db>`; the file is generated, not edited.
pub fn dump_schema(driver: &str) -> Result<String, String> {
    let path = get_project_root().join("schema.sql");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;

    let mut out = format!(
        "-- Generated from schema.sql for {driver} by `vlo schema --driver {driver}`.\n\
         -- Do not edit by hand: change schema.sql and regenerate.\n\n"
    );

    for stmt in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(sql) = translate_schema_statement(driver, stmt) {
            out.push_str(&sql);
            out.push_str(";\n\n");
        }
    }

    // Explicit ids in the seed rows do not advance a BIGSERIAL sequence.
    if driver == "postgres" {
        static SEEDED_ID_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"(?i)INSERT\s+(?:OR\s+IGNORE\s+)?INTO\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(\s*id\b").unwrap()
        });
        let mut tables: Vec<String> = Vec::new();
        for caps in SEEDED_ID_RE.captures_iter(&raw) {
            let table = caps[1].to_string();
            if !tables.contains(&table) {
                tables.push(table);
            }
        }
        if !tables.is_empty() {
            out.push_str("-- Advance BIGSERIAL sequences past explicitly seeded ids\n");
            for table in tables {
                out.push_str(&format!(
                    "SELECT setval(pg_get_serial_sequence('{t}', 'id'), COALESCE((SELECT MAX(id) FROM {t}), 1));\n",
                    t = table
                ));
            }
        }
    }

    Ok(out)
}

/// SQLite's `json_object` yields TEXT; Postgres' `jsonb_build_object` yields
/// JSONB, which cannot be assigned to a TEXT column without a cast. Appends
/// `::text` to each *outermost* call so nested objects keep their type.
fn pg_json_object_to_text(sql: &str) -> String {
    const NEEDLE: &str = "jsonb_build_object(";
    let mut out = String::with_capacity(sql.len() + 32);
    let mut rest = sql;

    while let Some(pos) = rest.find(NEEDLE) {
        let body_start = pos + NEEDLE.len();
        let Some(close) = matching_paren(rest.as_bytes(), body_start) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..=close]);
        out.push_str("::text");
        rest = &rest[close + 1..];
    }

    out.push_str(rest);
    out
}

/// `NULLIF(x, '')` relies on SQLite's dynamic typing. Postgres needs the
/// compared values to share a type, so make the first side text explicitly.
fn pg_nullif_empty_to_text(sql: &str) -> String {
    const NEEDLE: &str = "NULLIF(";
    let mut out = String::with_capacity(sql.len() + 32);
    let mut rest = sql;

    while let Some(pos) = rest.find(NEEDLE) {
        let body_start = pos + NEEDLE.len();
        let Some(close) = matching_paren(rest.as_bytes(), body_start) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..pos]);
        let inside = &rest[body_start..close];
        let rewritten = split_top_level_comma(inside).and_then(|(first, second)| {
            (second.trim() == "''").then(|| format!("NULLIF(CAST({} AS TEXT), '')", first.trim()))
        });
        match rewritten {
            Some(call) => out.push_str(&call),
            None => out.push_str(&rest[pos..=close]),
        }
        rest = &rest[close + 1..];
    }

    out.push_str(rest);
    out
}

/// Index of the `)` matching the `(` at `open`, ignoring quoted text.
fn matching_paren(bytes: &[u8], after_open: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut in_str = false;
    let mut i = after_open;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if in_str => {
                if bytes.get(i + 1) == Some(&b'\'') { i += 1; } else { in_str = false; }
            }
            b'\'' => in_str = true,
            b'(' if !in_str => depth += 1,
            b')' if !in_str => {
                depth -= 1;
                if depth == 0 { return Some(i); }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Splits `a, b` at the first top-level comma.
fn split_top_level_comma(body: &str) -> Option<(&str, &str)> {
    let bytes = body.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if in_str => {
                if bytes.get(i + 1) == Some(&b'\'') { i += 1; } else { in_str = false; }
            }
            b'\'' => in_str = true,
            b'(' if !in_str => depth += 1,
            b')' if !in_str => depth = depth.saturating_sub(1),
            b',' if !in_str && depth == 0 => return Some((&body[..i], &body[i + 1..])),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Maps a SQLite `strftime` template to the equivalent Postgres `to_char` one.
fn pg_date_format(fmt: &str) -> String {
    fmt.replace("%Y", "YYYY")
        .replace("%m", "MM")
        .replace("%d", "DD")
        .replace("%H", "HH24")
        .replace("%M", "MI")
        .replace("%S", "SS")
}

/// Maps a SQLite `strftime` template to MySQL's `DATE_FORMAT` template.
/// Only the tokens differ: MySQL uses `%i` for minutes and `%s` for seconds.
fn mysql_date_format(fmt: &str) -> String {
    let mut out = String::with_capacity(fmt.len() + 4);
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('M') => out.push_str("%i"),
            Some('S') => out.push_str("%s"),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// MySQL has no `||` concat operator (it means OR unless PIPES_AS_CONCAT is
/// set), so top-level `a || b || c` chains become `CONCAT(a, b, c)`.
fn mysql_concat(sql: &str) -> String {
    let mut out = sql.to_string();
    while let Some((start, end)) = find_concat_chain(&out) {
        let args = split_concat_chain(&out[start..end]);
        let replacement = format!("CONCAT({})", args.join(", "));
        out.replace_range(start..end, &replacement);
    }
    out
}

/// Byte range of the first `operand || operand [|| operand...]` chain.
/// VLO SQL uses `||` only for concatenation (logical OR is always `OR`), so
/// this scans at any paren depth and only has to skip quoted literals.
fn find_concat_chain(sql: &str) -> Option<(usize, usize)> {
    let bytes = sql.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i + 1 < bytes.len() {
        match bytes[i] {
            b'\'' => in_str = !in_str,
            b'|' if !in_str && bytes[i + 1] == b'|' => {
                let start = operand_start(sql, i);
                let mut end = operand_end(sql, i + 2);
                loop {
                    let mut next = end;
                    while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                        next += 1;
                    }
                    if next + 1 < bytes.len() && bytes[next] == b'|' && bytes[next + 1] == b'|' {
                        end = operand_end(sql, next + 2);
                    } else {
                        break;
                    }
                }
                return Some((start, end));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Start of the operand that ends just before `end` (quoted literal, function
/// call, parenthesised expression or bare identifier/number).
fn operand_start(sql: &str, end: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = end;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    match bytes[i - 1] {
        b')' => {
            let mut depth = 1usize;
            let mut j = i - 1;
            while j > 0 {
                j -= 1;
                if bytes[j] == b')' {
                    depth += 1;
                } else if bytes[j] == b'(' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            let mut k = j;
            while k > 0 && (bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_') {
                k -= 1;
            }
            k
        }
        b'\'' => {
            let mut j = i - 1;
            while j > 0 {
                j -= 1;
                if bytes[j] == b'\'' {
                    break;
                }
            }
            j
        }
        _ => {
            let mut k = i;
            while k > 0 && is_operand_byte(bytes[k - 1]) {
                k -= 1;
            }
            k
        }
    }
}

/// End (exclusive) of the operand starting at `start`.
fn operand_end(sql: &str, start: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return i;
    }
    if bytes[i] == b'\'' {
        i += 1;
        while i < bytes.len() {
            if bytes[i] == b'\'' {
                i += 1;
                break;
            }
            i += 1;
        }
        return i;
    }
    if bytes[i] == b'(' {
        return skip_balanced_parens(bytes, i);
    }
    while i < bytes.len() && is_operand_byte(bytes[i]) {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'(' {
        return skip_balanced_parens(bytes, i);
    }
    i
}

fn is_operand_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'$' | b'?' | b'{' | b'}')
}

fn skip_balanced_parens(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// Splits `a || b || c` into its top-level operands.
fn split_concat_chain(chain: &str) -> Vec<String> {
    let bytes = chain.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        match bytes[i] {
            b'\'' => in_str = !in_str,
            b'(' if !in_str => depth += 1,
            b')' if !in_str => depth -= 1,
            b'|' if !in_str && depth == 0 && bytes[i + 1] == b'|' => {
                parts.push(chain[start..i].trim().to_string());
                i += 2;
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(chain[start..].trim().to_string());
    parts
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
            let Some(stmt) = pool.prepare_schema_statement(stmt) else { continue };
            count += 1;
            if let Err(err) = pool.execute(&stmt).await { crate::vlo_debug!("⚠️ Schema statement failed: {}", err); }
        }
        crate::vlo_debug!("✅ {} processed ({} statements)", schema_file, count);
    }

    if DB_POOL.set(pool).is_ok() { crate::vlo_debug!("✅ Database pool initialized"); }
    Ok(())
}