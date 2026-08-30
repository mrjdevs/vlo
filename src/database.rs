use crate::state::get_project_root;
use std::{fs, sync::OnceLock};

// ============================================================
// 9. VLO DATABASE
// ============================================================

#[derive(Clone)]
pub enum DbPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
}

pub static DB_POOL: OnceLock<DbPool> = OnceLock::new();

pub async fn init_db() {
    dotenvy::dotenv().ok();

    let db_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("⚠️ DATABASE_URL not set. DB features disabled.");
            return;
        }
    };

    let driver = std::env::var("DB_DRIVER")
        .unwrap_or_else(|_| "sqlite".to_string());

    let pool = match driver.to_lowercase().as_str() {
        "postgres" | "postgresql" => {
            DbPool::Postgres(
                sqlx::PgPool::connect(&db_url)
                    .await
                    .expect("Failed to connect to Postgres"),
            )
        }

        "mysql" => {
            DbPool::MySql(
                sqlx::MySqlPool::connect(&db_url)
                    .await
                    .expect("Failed to connect to MySQL"),
            )
        }

        _ => {
            use std::str::FromStr;

            let options = sqlx::sqlite::SqliteConnectOptions::from_str(&db_url)
                .unwrap_or_else(|_| sqlx::sqlite::SqliteConnectOptions::new())
                .create_if_missing(true);

            let p = sqlx::sqlite::SqlitePoolOptions::new()
                .connect_with(options)
                .await
                .expect("Failed to connect to SQLite");

            let _ = sqlx::query("PRAGMA foreign_keys = ON;")
                .execute(&p)
                .await;

            let _ = sqlx::query("PRAGMA journal_mode = WAL;")
                .execute(&p)
                .await;

            DbPool::Sqlite(p)
        }
    };

    let root = get_project_root();
    let schema_path = root.join("schema.sql");

    if schema_path.exists() {
        let sql = fs::read_to_string(&schema_path)
            .expect("Failed to read schema.sql");

        for statement in sql.split(';') {
            let stmt = statement.trim();

            if stmt.is_empty()
                || stmt.to_uppercase().starts_with("INSERT")
            {
                continue;
            }

            match &pool {
                DbPool::Sqlite(p) => {
                    let _ = sqlx::query(stmt).execute(p).await;
                }
                DbPool::Postgres(p) => {
                    let _ = sqlx::query(stmt).execute(p).await;
                }
                DbPool::MySql(p) => {
                    let _ = sqlx::query(stmt).execute(p).await;
                }
            }
        }
    }

    let _ = DB_POOL.set(pool);
}