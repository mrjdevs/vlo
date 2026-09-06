use crate::state::get_project_root;
use std::{fs, path::Path, sync::OnceLock};

#[derive(Clone)]
pub enum DbPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
}

pub static DB_POOL: OnceLock<DbPool> = OnceLock::new();

pub async fn init_db() -> Result<(), String> {
    let root = get_project_root();
    let env_path = root.join(".env");

    if env_path.exists() {
        if let Err(err) = dotenvy::from_path(&env_path) {
            eprintln!("⚠️ Failed to load .env: {}", err);
        } else {
            crate::vlo_debug!("🔧 VLO DEBUG: Loaded .env from {}", env_path.display());
        }
    } else {
        crate::vlo_debug!("⚠️ VLO DEBUG: .env not found at {}", env_path.display());
    }

    crate::vlo_debug!("🔧 VLO DEBUG: Project root = {}", root.display());

    let db_url = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            crate::vlo_debug!("🔧 VLO DEBUG: DATABASE_URL = {}", url);
            url
        }
        _ => {
            eprintln!("⚠️ DATABASE_URL not set. DB features disabled.");
            return Ok(());
        }
    };

    let driver = std::env::var("DB_DRIVER")
        .unwrap_or_else(|_| "sqlite".to_string())
        .to_lowercase();

    crate::vlo_debug!("🔧 VLO DEBUG: DB_DRIVER = {}", driver);

    let pool = match driver.as_str() {
        "postgres" | "postgresql" => {
            crate::vlo_debug!("🔧 VLO DEBUG: Connecting to PostgreSQL...");

            let pool = sqlx::PgPool::connect(&db_url)
                .await
                .map_err(|err| format!("Failed to connect to PostgreSQL: {}", err))?;

            crate::vlo_debug!("✅ VLO DEBUG: PostgreSQL connected");
            DbPool::Postgres(pool)
        }

        "mysql" => {
            crate::vlo_debug!("🔧 VLO DEBUG: Connecting to MySQL...");

            let pool = sqlx::MySqlPool::connect(&db_url)
                .await
                .map_err(|err| format!("Failed to connect to MySQL: {}", err))?;

            crate::vlo_debug!("✅ VLO DEBUG: MySQL connected");
            DbPool::MySql(pool)
        }

        _ => {
            crate::vlo_debug!("🔧 VLO DEBUG: Connecting to SQLite...");

            let db_path = db_url.strip_prefix("sqlite://").unwrap_or(&db_url);
            let path = Path::new(db_path);
            let absolute_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };

            crate::vlo_debug!(
                "🔧 VLO DEBUG: SQLite database path = {}",
                absolute_path.display()
            );

            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&absolute_path)
                .create_if_missing(true);

            let pool = sqlx::SqlitePool::connect_with(options)
                .await
                .map_err(|err| {
                    crate::vlo_debug!(
                        "🔧 VLO DEBUG: SQLite path attempted = {}",
                        absolute_path.display()
                    );
                    format!("Failed to connect to SQLite: {}", err)
                })?;

            crate::vlo_debug!(
                "✅ VLO DEBUG: SQLite connected: {}",
                absolute_path.display()
            );

            DbPool::Sqlite(pool)
        }
    };

    let schema_path = root.join("schema.sql");

    crate::vlo_debug!(
        "🔧 VLO DEBUG: Schema path = {}",
        schema_path.display()
    );

    if schema_path.exists() {
        crate::vlo_debug!("🔧 VLO DEBUG: Loading schema.sql...");

        let sql = fs::read_to_string(&schema_path)
            .map_err(|err| format!("Failed to read schema.sql: {}", err))?;

        let mut statement_count = 0usize;

        for statement in sql.split(';') {
            let stmt = statement.trim();

            if stmt.is_empty() || stmt.to_uppercase().starts_with("INSERT") {
                continue;
            }

            statement_count += 1;

            match &pool {
                DbPool::Sqlite(p) => {
                    if let Err(err) = sqlx::query(stmt).execute(p).await {
                        crate::vlo_debug!(
                            "⚠️ VLO DEBUG: SQLite schema statement failed: {}",
                            err
                        );
                    }
                }

                DbPool::Postgres(p) => {
                    if let Err(err) = sqlx::query(stmt).execute(p).await {
                        crate::vlo_debug!(
                            "⚠️ VLO DEBUG: PostgreSQL schema statement failed: {}",
                            err
                        );
                    }
                }

                DbPool::MySql(p) => {
                    if let Err(err) = sqlx::query(stmt).execute(p).await {
                        crate::vlo_debug!(
                            "⚠️ VLO DEBUG: MySQL schema statement failed: {}",
                            err
                        );
                    }
                }
            }
        }

        crate::vlo_debug!(
            "✅ VLO DEBUG: schema.sql processed ({} statements)",
            statement_count
        );
    } else {
        crate::vlo_debug!(
            "⚠️ VLO DEBUG: schema.sql not found at {}",
            schema_path.display()
        );
    }

    if DB_POOL.set(pool).is_ok() {
        crate::vlo_debug!("✅ VLO DEBUG: Database pool initialized");
    } else {
        crate::vlo_debug!("⚠️ VLO DEBUG: Database pool was already initialized");
    }

    Ok(())
}