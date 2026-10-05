use crate::state::get_project_root;
use serde_json::Value;
use std::{fs, path::Path, sync::OnceLock};
use sqlx::{Column, Row, ValueRef};

#[derive(Clone)]
pub enum DbPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
}

pub static DB_POOL: OnceLock<DbPool> = OnceLock::new();

// ---------------------------------------------------------------------------
// JSON Mapping Macro (Moved from api.rs)
// ---------------------------------------------------------------------------
macro_rules! convert_row_to_json {
    ($row:expr) => {{
        let mut map = serde_json::Map::new();
        for (i, column) in $row.columns().iter().enumerate() {
            let name = column.name().to_string();
            let is_null = $row.try_get_raw(i).map(|r| r.is_null()).unwrap_or(true);
            
            let val: serde_json::Value = if is_null {
                serde_json::Value::Null
            } else {
                // Try types in order of likelihood. We ignore column.type_info()
                // because SQLite dynamically reports 'null' for things like COUNT(*).
                if let Ok(v) = $row.try_get::<i64, _>(i) {
                    serde_json::Value::Number(v.into())
                } else if let Ok(v) = $row.try_get::<i32, _>(i) {
                    serde_json::Value::Number((v as i64).into())
                } else if let Ok(v) = $row.try_get::<f64, _>(i) {
                    serde_json::Number::from_f64(v).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null)
                } else if let Ok(v) = $row.try_get::<bool, _>(i) {
                    serde_json::Value::Bool(v)
                } else if let Ok(v) = $row.try_get::<String, _>(i) {
                    if let Ok(n) = v.parse::<i64>() {
                        serde_json::Value::Number(n.into())
                    } else if let Ok(n) = v.parse::<f64>() {
                        serde_json::Number::from_f64(n).map(serde_json::Value::Number).unwrap_or(serde_json::Value::String(v))
                    } else {
                        serde_json::Value::String(v)
                    }
                } else if let Ok(v) = $row.try_get::<Vec<u8>, _>(i) {
                    serde_json::Value::String(format!("blob {}b", v.len()))
                } else {
                    serde_json::Value::Null
                }
            };
            map.insert(name, val);
        }
        serde_json::Value::Object(map)
    }};
}

// ---------------------------------------------------------------------------
// DbPool Helper Methods
// ---------------------------------------------------------------------------
impl DbPool {
    /// Connects to the database based on the driver string
    pub async fn connect(driver: &str, url: &str, root: &Path) -> Result<Self, String> {
        match driver {
            "postgres" | "postgresql" => {
                sqlx::PgPool::connect(url).await
                    .map(DbPool::Postgres)
                    .map_err(|e| format!("Failed to connect to PostgreSQL: {}", e))
            }
            "mysql" => {
                sqlx::MySqlPool::connect(url).await
                    .map(DbPool::MySql)
                    .map_err(|e| format!("Failed to connect to MySQL: {}", e))
            }
            _ => {
                let db_path = url.strip_prefix("sqlite://").unwrap_or(url);
                let path = Path::new(db_path);
                let absolute_path = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
                
                let options = sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&absolute_path)
                    .create_if_missing(true);
                    
                sqlx::SqlitePool::connect_with(options).await
                    .map(DbPool::Sqlite)
                    .map_err(|e| format!("Failed to connect to SQLite: {}", e))
            }
        }
    }

    /// Executes a SQL statement (DDL, INSERT, UPDATE, DELETE) without returning rows
    pub async fn execute(&self, sql: &str) -> Result<u64, sqlx::Error> {
        match self {
            DbPool::Sqlite(p) => sqlx::query(sql).execute(p).await.map(|r| r.rows_affected()),
            DbPool::Postgres(p) => sqlx::query(sql).execute(p).await.map(|r| r.rows_affected()),
            DbPool::MySql(p) => sqlx::query(sql).execute(p).await.map(|r| r.rows_affected()),
        }
    }

    /// Fetches all rows and converts them to a JSON array
    pub async fn fetch_all_json(&self, sql: &str) -> Result<Vec<Value>, sqlx::Error> {
        match self {
            DbPool::Sqlite(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await?;
                Ok(rows.iter().map(|row| convert_row_to_json!(row)).collect())
            }
            DbPool::Postgres(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await?;
                Ok(rows.iter().map(|row| convert_row_to_json!(row)).collect())
            }
            DbPool::MySql(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await?;
                Ok(rows.iter().map(|row| convert_row_to_json!(row)).collect())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------
pub async fn init_db() -> Result<(), String> {
    let root = get_project_root();
    let env_path = root.join(".env");

    if env_path.exists() {
        if let Err(err) = dotenvy::from_path(&env_path) {
            eprintln!("⚠️ Failed to load .env: {}", err);
        }
    }

    let db_url = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("⚠️ DATABASE_URL not set. DB features disabled.");
            return Ok(());
        }
    };

    let driver = std::env::var("DB_DRIVER")
        .unwrap_or_else(|_| "sqlite".to_string())
        .to_lowercase();

    // Use the new helper method
    let pool = DbPool::connect(&driver, &db_url, &root).await?;

    // Load schema filename from DB_SCHEMA env var, defaulting to "schema.sql"
    let schema_filename = std::env::var("DB_SCHEMA")
        .unwrap_or_else(|_| "schema.sql".to_string());

    let schema_path = root.join(&schema_filename);

    if schema_path.exists() {
        let sql = fs::read_to_string(&schema_path)
            .map_err(|err| format!("Failed to read {}: {}", schema_filename, err))?;

        let mut statement_count = 0usize;

        for statement in sql.split(';') {
            let stmt = statement.trim();
            if stmt.is_empty() || stmt.to_uppercase().starts_with("INSERT") {
                continue;
            }

            statement_count += 1;
            
            // Use the new helper method
            if let Err(err) = pool.execute(stmt).await {
                crate::vlo_debug!("⚠️ Schema statement failed: {}", err);
            }
        }

        crate::vlo_debug!("✅ {} processed ({} statements)", schema_filename, statement_count);
    }

    if DB_POOL.set(pool).is_ok() {
        crate::vlo_debug!("✅ Database pool initialized");
    }

    Ok(())
}