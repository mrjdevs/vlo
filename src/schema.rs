// src/schema.rs
use crate::database::DbPool;

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub sql_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
    pub unique: bool,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, sql_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sql_type: sql_type.into(),
            nullable: true,
            default: None,
            primary_key: false,
            unique: false,
        }
    }
    pub fn primary_key(mut self) -> Self { self.primary_key = true; self.nullable = false; self }
    pub fn not_null(mut self) -> Self { self.nullable = false; self }
    pub fn default(mut self, d: impl Into<String>) -> Self { self.default = Some(d.into()); self }
    #[allow(dead_code)]
    pub fn unique(mut self) -> Self { self.unique = true; self }
}

pub async fn ensure_table(pool: &DbPool, table_name: &str, columns: &[ColumnDef]) -> Result<(), String> {
    let exists = table_exists(pool, table_name).await?;

    if !exists {
        let create_sql = build_create(pool, table_name, columns);
        crate::vlo_debug!("🔧 [SCHEMA] Creating table '{}'", table_name);
        pool.execute(&create_sql)
            .await
            .map_err(|e| format!("Failed to create table {}: {}", table_name, e))?;
        return Ok(());
    }

    let current = current_columns(pool, table_name).await?;

    for col in columns {
        if current.iter().any(|c| c == &col.name) {
            continue;
        }

        if col.primary_key || col.unique {
            crate::vlo_debug!(
                "⚠️ [SCHEMA] Column {}.{} missing with PK/UNIQUE — manual migration required",
                table_name, col.name
            );
            continue;
        }

        let add_sql = if !col.nullable && col.default.is_none() {
            crate::vlo_debug!(
                "⚠️ [SCHEMA] Adding {}.{} as nullable (was NOT NULL without default)",
                table_name, col.name
            );
            format!("ALTER TABLE {} ADD COLUMN {} {}", table_name, col.name, col.sql_type)
        } else {
            let mut s = format!("ALTER TABLE {} ADD COLUMN {} {}", table_name, col.name, col.sql_type);
            if let Some(d) = &col.default {
                s.push_str(&format!(" DEFAULT {}", d));
            }
            s
        };

        match pool.execute(&add_sql).await {
            Ok(_) => crate::vlo_debug!("✅ [SCHEMA] Added column {}.{}", table_name, col.name),
            Err(e) => crate::vlo_debug!(
                "⚠️ [SCHEMA] Failed to add {}.{}: {} — manual fix required",
                table_name, col.name, e
            ),
        }
    }

    Ok(())
}

async fn table_exists(pool: &DbPool, table_name: &str) -> Result<bool, String> {
    let sql = pool.table_exists_sql(table_name);
    let rows = pool.fetch_all_json(&sql).await.map_err(|e| e.to_string())?;
    Ok(!rows.is_empty())
}

async fn current_columns(pool: &DbPool, table_name: &str) -> Result<Vec<String>, String> {
    let sql = pool.table_columns_sql(table_name);
    let key = pool.table_columns_key();
    let rows = pool.fetch_all_json(&sql).await.map_err(|e| e.to_string())?;
    Ok(rows.iter()
        .filter_map(|r| r.get(key).and_then(|v| v.as_str()).map(String::from))
        .collect())
}

fn build_create(pool: &DbPool, table_name: &str, columns: &[ColumnDef]) -> String {
    let cols = columns.iter().map(|c| {
        let auto_int = c.primary_key && c.sql_type.eq_ignore_ascii_case("INTEGER");
        // Postgres spells auto-increment as a type; MySQL needs BIGINT so ids
        // read as i64 and foreign keys match the referenced column type.
        let mut s = match (pool, auto_int) {
            (DbPool::Postgres(_), true) => format!("{} BIGSERIAL", c.name),
            (DbPool::MySql(_), true) => format!("{} BIGINT", c.name),
            _ => format!("{} {}", c.name, c.sql_type),
        };
        if c.primary_key {
            match pool {
                DbPool::Sqlite(_) if auto_int => s.push_str(" PRIMARY KEY AUTOINCREMENT"),
                DbPool::MySql(_) if auto_int => s.push_str(" AUTO_INCREMENT PRIMARY KEY"),
                _ => s.push_str(" PRIMARY KEY"),
            }
        }
        if !c.nullable && !c.primary_key { s.push_str(" NOT NULL"); }
        if let Some(d) = &c.default { s.push_str(&format!(" DEFAULT {}", d)); }
        if c.unique && !c.primary_key { s.push_str(" UNIQUE"); }
        s
    }).collect::<Vec<_>>().join(", ");
    format!("CREATE TABLE IF NOT EXISTS {} ({})", table_name, cols)
}