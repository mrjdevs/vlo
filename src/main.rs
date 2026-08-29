use async_stream::stream;
use axum::{
    extract::{Path as AxumPath, Query},
    http::{Method, StatusCode},
    response::{
        sse::{Event, KeepAlive},
        Html, IntoResponse, Json, Sse,
    },
    routing::get,
    Router,
};
use clap::{Parser, Subcommand};
use futures_util::stream::Stream;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use serde_json::Value;
use sqlx::{Arguments, Column, Row, TypeInfo, ValueRef};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc::channel, Arc, LazyLock, Mutex, OnceLock},
    time::Instant,
};
use tokio::sync::broadcast;
use tower_http::services::ServeDir;

// ============================================================
// VLO CLI DEFINITIONS & DOCUMENTATION
// ============================================================

#[derive(Parser)]
#[command(
    name = "vlo",
    author = "VLO Team",
    version,
    about = "⚡ VLO - Ultra-fast, component-driven Web Framework powered by Rust & Axum",
    long_about = "VLO combines component rendering, instant dynamic SQL APIs, hot module reloading (HMR), and simple static deployment into a single high-performance CLI framework."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new VLO project with sample pages, layout, and database setup
    #[command(name = "init", alias = "new")]
    Init {
        /// Project folder name or path (defaults to current directory '.')
        #[arg(default_value = ".")]
        name: String,

        /// Database driver to setup (sqlite, postgres, mysql)
        #[arg(short, long, default_value = "sqlite", value_parser = ["sqlite", "postgres", "mysql"])]
        db: String,

        /// Explicit database name or filename (e.g. custom_app.db or my_database)
        #[arg(long = "db-name", alias = "db_name")]
        db_name: Option<String>,

        /// Disable initial database setup and configuration
        #[arg(long)]
        no_db: bool,
    },

    /// Start the local development server with Hot Module Reloading (HMR)
    Dev {
        /// Port to listen on (default: 3000)
        #[arg(short, long, default_value = "3000")]
        port: u16,

        /// Host address to bind (default: 127.0.0.1)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },

    /// Build static HTML files and production assets for distribution
    Build,

    /// Deploy the project to cloud hosting platforms (netlify, vercel, cloudflare, railway)
    Deploy {
        /// Cloud provider target for deployment
        #[arg(short, long, default_value = "netlify", value_parser = ["netlify", "vercel", "cloudflare", "pages", "railway"])]
        provider: String,
    },
}

// ============================================================
// APPLICATION ENTRY
// ============================================================

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init {
            ref name,
            ref db,
            ref db_name,
            no_db,
        } => {
            init_project(name, db, db_name.as_deref(), no_db);
        }
        Commands::Dev { port, ref host } => {
            init_db().await;
            dev(host, port).await;
        }
        Commands::Build => {
            init_db().await;
            build();
        }
        Commands::Deploy { ref provider } => {
            init_db().await;
            deploy(provider).await;
        }
    }
}

// ============================================================
// PROJECT BOILERPLATE GENERATOR (vlo init)
// ============================================================

fn init_project(name: &str, db_driver: &str, db_name_opt: Option<&str>, no_db: bool) {
    let target_dir = if name == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(name)
    };

    println!("⚡ Initializing VLO project in '{}'...", target_dir.display());

    if target_dir != Path::new(".") && !target_dir.exists() {
        fs::create_dir_all(&target_dir).expect("Failed to create project directory");
    }

    let pages_dir = target_dir.join("pages");
    let api_dir = pages_dir.join("api");
    let layouts_dir = target_dir.join("layouts");
    let components_dir = target_dir.join("components");
    let public_dir = target_dir.join("public");

    fs::create_dir_all(&api_dir).ok();
    fs::create_dir_all(&layouts_dir).ok();
    fs::create_dir_all(&components_dir).ok();
    fs::create_dir_all(&public_dir).ok();

    // Priority for database naming: 1) explicit --db-name flag, 2) derived project folder name, 3) fallback app name
    let db_name = match db_name_opt {
        Some(custom) if !custom.trim().is_empty() => custom.trim().to_string(),
        _ => {
            let base_name = if name != "." {
                Path::new(name)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("app")
            } else {
                "app"
            };
            let sanitized = base_name.replace(|c: char| !c.is_alphanumeric() && c != '_' && c != '-', "_");
            if db_driver == "sqlite" {
                if sanitized.ends_with(".db") || sanitized.ends_with(".sqlite") {
                    sanitized
                } else {
                    format!("{}.db", sanitized)
                }
            } else {
                sanitized
            }
        }
    };

    // Create .env
    let env_path = target_dir.join(".env");
    if !env_path.exists() {
        let env_content = if no_db {
            "# Database disabled\n# DB_DRIVER=sqlite\n# DATABASE_URL=sqlite://app.db\n".to_string()
        } else {
            match db_driver.to_lowercase().as_str() {
                "postgres" | "postgresql" => {
                    format!("DB_DRIVER=postgres\nDATABASE_URL=postgres://postgres:password@localhost:5432/{}\n", db_name)
                }
                "mysql" => {
                    format!("DB_DRIVER=mysql\nDATABASE_URL=mysql://root:password@localhost:3306/{}\n", db_name)
                }
                _ => {
                    format!("DB_DRIVER=sqlite\nDATABASE_URL=sqlite://{}\n", db_name)
                }
            }
        };
        fs::write(&env_path, env_content).expect("Failed to write .env");
    }

    if !no_db && matches!(db_driver.to_lowercase().as_str(), "sqlite" | "") {
        let db_file_path = target_dir.join(&db_name);
        if !db_file_path.exists() {
            fs::File::create(&db_file_path).ok();
        }
    }

    // Create schema.sql with initial seed dummy data (3-5 items)
    let schema_path = target_dir.join("schema.sql");
    if !schema_path.exists() && !no_db {
        let schema_sql = match db_driver.to_lowercase().as_str() {
            "postgres" | "postgresql" => {
                r#"CREATE TABLE IF NOT EXISTS items (
    id SERIAL PRIMARY KEY,
    title TEXT NOT NULL
);

INSERT INTO items (title) VALUES ('⚡ Learn VLO Framework Architecture');
INSERT INTO items (title) VALUES ('🛠️ Explore Component & Slot Composition');
INSERT INTO items (title) VALUES ('🚀 Build Zero-Boilerplate Dynamic APIs');
INSERT INTO items (title) VALUES ('📦 Deploy to Cloud Platforms in One Step');
"#
            }
            "mysql" => {
                r#"CREATE TABLE IF NOT EXISTS items (
    id INT AUTO_INCREMENT PRIMARY KEY,
    title VARCHAR(255) NOT NULL
);

INSERT INTO items (title) VALUES 
('⚡ Learn VLO Framework Architecture'),
('🛠️ Explore Component & Slot Composition'),
('🚀 Build Zero-Boilerplate Dynamic APIs'),
('📦 Deploy to Cloud Platforms in One Step');
"#
            }
            _ => {
                r#"CREATE TABLE IF NOT EXISTS items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL
);

INSERT INTO items (title) VALUES ('⚡ Learn VLO Framework Architecture');
INSERT INTO items (title) VALUES ('🛠️ Explore Component & Slot Composition');
INSERT INTO items (title) VALUES ('🚀 Build Zero-Boilerplate Dynamic APIs');
INSERT INTO items (title) VALUES ('📦 Deploy to Cloud Platforms in One Step');
"#
            }
        };
        fs::write(&schema_path, schema_sql).expect("Failed to write schema.sql");
    }

    // Create pages/api/api.vlo
    let api_vlo_path = api_dir.join("api.vlo");
    if !api_vlo_path.exists() {
        let api_content = r#"<script server>
{
    "get_items": "SELECT * FROM items ORDER BY id DESC;",
    "post_items": "INSERT INTO items (title) VALUES ({{title}});",
    "put_items": "UPDATE items SET title = {{title}} WHERE id = {{id}};",
    "delete_items": "DELETE FROM items WHERE id = {{id}};"
}
</script>"#;
        fs::write(&api_vlo_path, api_content).expect("Failed to write api.vlo");
    }

    // Create layouts/BaseLayout.vlo
    let layout_path = layouts_dir.join("BaseLayout.vlo");
    if !layout_path.exists() {
        let layout_content = r#"<div class="layout-container">
    <header class="app-header">
        <div class="brand">
            <h1>⚡ {{title}}</h1>
        </div>
        <nav class="app-nav">
            <a href="/">Home</a>
            <a href="/about">About</a>
        </nav>
    </header>

    <main class="app-main">
        <slot></slot>
    </main>

    <footer class="app-footer">
        <p>Built with ⚡ <strong>VLO Web Framework</strong></p>
    </footer>
</div>

<style>
    .layout-container { max-width: 720px; margin: 0 auto; padding: 2.5rem 1.5rem; }
    .app-header { display: flex; justify-content: space-between; align-items: center; border-bottom: 1px solid #334155; padding-bottom: 1.25rem; margin-bottom: 2rem; }
    .app-header h1 { font-size: 1.5rem; font-weight: 700; margin: 0; color: #f8fafc; }
    .app-nav { display: flex; gap: 1.25rem; }
    .app-nav a { color: #94a3b8; text-decoration: none; font-weight: 500; font-size: 0.95rem; transition: color 0.15s ease; }
    .app-nav a:hover { color: #38bdf8; }
    .app-main { min-height: 320px; }
    .app-footer { border-top: 1px solid #334155; padding-top: 1.5rem; margin-top: 3rem; text-align: center; color: #64748b; font-size: 0.875rem; }
    .app-footer strong { color: #cbd5e1; }
</style>"#;
        fs::write(&layout_path, layout_content).expect("Failed to write BaseLayout.vlo");
    }

    // Component 1: Card.vlo
    let card_path = components_dir.join("Card.vlo");
    if !card_path.exists() {
        let card_content = r#"<div class="vlo-card">
    <slot></slot>
</div>

<style>
    .vlo-card { background: #1e293b; border: 1px solid #334155; border-radius: 12px; padding: 1.5rem; box-shadow: 0 4px 6px -1px rgba(0, 0, 0, 0.3); margin-bottom: 1.5rem; }
</style>"#;
        fs::write(&card_path, card_content).expect("Failed to write Card.vlo");
    }

    // Component 2: Badge.vlo
    let badge_path = components_dir.join("Badge.vlo");
    if !badge_path.exists() {
        let badge_content = r#"<span class="vlo-badge">
    <slot></slot>
</span>

<style>
    .vlo-badge {
        display: inline-flex;
        align-items: center;
        padding: 0.25rem 0.65rem;
        font-size: 0.75rem;
        font-weight: 600;
        border-radius: 9999px;
        background: rgba(56, 189, 248, 0.15);
        color: #38bdf8;
        border: 1px solid rgba(56, 189, 248, 0.3);
    }
</style>"#;
        fs::write(&badge_path, badge_content).expect("Failed to write Badge.vlo");
    }

    // Create pages/home.vlo
    let home_path = pages_dir.join("home.vlo");
    if !home_path.exists() {
        let home_content = r#"<BaseLayout title="VLO Dashboard">
    <Card>
        <div class="card-header">
            <h2>Items (Live SQL CRUD)</h2>
            <Badge>Connected to API</Badge>
        </div>

        <form onsubmit="addItem(event)" class="crud-form">
            <input id="item-title" placeholder="Add new task or item..." required />
            <button type="submit">+ Add</button>
        </form>

        <ul id="items-list">
            <li class="empty">Loading data from database...</li>
        </ul>
    </Card>
</BaseLayout>

<script>
    document.addEventListener("DOMContentLoaded", loadItems);

    async function loadItems() {
        const list = document.getElementById("items-list");
        try {
            const res = await fetch("/api/items");
            const { data } = await res.json();
            if (!data || !data.length) {
                list.innerHTML = '<li class="empty">No items found in database. Add one above!</li>';
                return;
            }
            list.innerHTML = data.map(item => `
                <li>
                    <span class="item-text" onclick="editItem(${item.id}, '${escapeJs(item.title)}')" title="Click to edit">
                        ${escapeHtml(item.title)}
                    </span>
                    <button class="btn-del" onclick="deleteItem(${item.id})" title="Delete">✕</button>
                </li>
            `).join("");
        } catch (e) {
            list.innerHTML = '<li class="empty text-error">Failed to load items from API.</li>';
        }
    }

    async function addItem(e) {
        e.preventDefault();
        const input = document.getElementById("item-title");
        const title = input.value.trim();
        if (!title) return;

        await fetch("/api/items", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ title })
        });
        input.value = "";
        loadItems();
    }

    async function editItem(id, currentTitle) {
        const newTitle = prompt("Update item title:", currentTitle);
        if (!newTitle || newTitle.trim() === currentTitle) return;

        await fetch(`/api/items/${id}`, {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ title: newTitle.trim() })
        });
        loadItems();
    }

    async function deleteItem(id) {
        if (!confirm("Are you sure you want to delete this item?")) return;
        await fetch(`/api/items/${id}`, { method: "DELETE" });
        loadItems();
    }

    function escapeHtml(s) { return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }
    function escapeJs(s) { return String(s).replace(/'/g, "\\'").replace(/"/g, '\\"'); }
</script>

<style>
    .card-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 1.25rem; }
    .card-header h2 { font-size: 1.25rem; font-weight: 600; margin: 0; color: #f8fafc; }
    
    .crud-form { display: flex; gap: 0.75rem; margin-bottom: 1.5rem; }
    .crud-form input { flex: 1; padding: 0.625rem 0.875rem; background: #0f172a; border: 1px solid #334155; border-radius: 8px; font-size: 0.95rem; color: #f8fafc; outline: none; transition: border-color 0.15s; }
    .crud-form input:focus { border-color: #38bdf8; }
    .crud-form button { padding: 0.625rem 1.25rem; background: #38bdf8; color: #0f172a; border: none; border-radius: 8px; font-weight: 600; cursor: pointer; transition: background-color 0.15s; }
    .crud-form button:hover { background: #7dd3fc; }

    #items-list { list-style: none; padding: 0; margin: 0; border: 1px solid #334155; border-radius: 8px; overflow: hidden; background: #0f172a; }
    #items-list li { display: flex; justify-content: space-between; align-items: center; padding: 0.875rem 1rem; border-bottom: 1px solid #1e293b; color: #e2e8f0; }
    #items-list li:last-child { border-bottom: none; }
    
    .item-text { cursor: pointer; flex: 1; font-size: 0.95rem; }
    .item-text:hover { color: #38bdf8; text-decoration: underline; }
    
    .btn-del { background: transparent; border: none; color: #64748b; cursor: pointer; font-size: 0.95rem; padding: 0.25rem 0.5rem; border-radius: 4px; transition: all 0.15s; }
    .btn-del:hover { color: #f87171; background: rgba(248, 113, 113, 0.1); }
    
    .empty { color: #64748b; text-align: center; font-size: 0.9rem; justify-content: center !important; }
    .text-error { color: #f87171; }
</style>"#;
        fs::write(&home_path, home_content).expect("Failed to write home.vlo");
    }

    // Create pages/about.vlo
    let about_path = pages_dir.join("about.vlo");
    if !about_path.exists() {
        let about_content = r#"<BaseLayout title="About VLO">
    <Card>
        <div class="card-header">
            <h2>About This Framework</h2>
            <Badge>v0.6.0</Badge>
        </div>
        <p>This application was scaffolded with ⚡ <strong>VLO</strong>, a high-performance component-driven Rust web framework.</p>
        <p>Out of the box capabilities:</p>
        <ul>
            <li>Automatic file-based API endpoints (<code>/api/...</code>)</li>
            <li>Zero-boilerplate multi-database dynamic SQL engines</li>
            <li>Hot Module Replacement (HMR) for live developer feedback</li>
            <li>Modular component architecture (<code>&lt;Card&gt;</code>, <code>&lt;Badge&gt;</code>, etc.)</li>
        </ul>
    </Card>
</BaseLayout>

<style>
    .card-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 1rem; }
    .card-header h2 { font-size: 1.25rem; font-weight: 600; margin: 0; color: #f8fafc; }
    p { color: #cbd5e1; }
    ul { color: #94a3b8; padding-left: 1.25rem; }
    li { margin-bottom: 0.5rem; }
</style>"#;
        fs::write(&about_path, about_content).expect("Failed to write about.vlo");
    }

    // Create pages/404.vlo
    let not_found_path = pages_dir.join("404.vlo");
    if !not_found_path.exists() {
        let not_found_content = r#"<BaseLayout title="404 - Not Found">
    <Card>
        <h2>404 - Page Not Found</h2>
        <p>The page you are looking for does not exist.</p>
        <a href="/">← Return to Home</a>
    </Card>
</BaseLayout>

<style>
    h2 { color: #f8fafc; margin-top: 0; }
    p { color: #94a3b8; }
</style>"#;
        fs::write(&not_found_path, not_found_content).expect("Failed to write 404.vlo");
    }

    // Create public/style.css with sleek high-contrast dark theme defaults
    let css_path = public_dir.join("style.css");
    if !css_path.exists() {
        let css_content = r#"*, *::before, *::after { box-sizing: border-box; }
body {
    margin: 0;
    padding: 0;
    background-color: #0f172a;
    color: #f8fafc;
    font-family: system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
    line-height: 1.6;
}
a { color: #38bdf8; text-decoration: none; }
a:hover { text-decoration: underline; }"#;
        fs::write(&css_path, css_content).expect("Failed to write style.css");
    }

    println!("✅ Project initialized successfully with database target '{}'!", db_name);
    if name != "." {
        println!("\nNext steps:\n  cd {}\n  vlo dev", name);
    } else {
        println!("\nNext steps:\n  vlo dev");
    }
}

// ============================================================
// DATABASE POOL (Multi-DB Support)
// ============================================================

#[derive(Clone)]
pub enum DbPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
}

static DB_POOL: OnceLock<DbPool> = OnceLock::new();

async fn init_db() {
    dotenvy::dotenv().ok();

    let db_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("⚠️ DATABASE_URL not set in .env. Database features will be disabled.");
            return;
        }
    };

    let driver = std::env::var("DB_DRIVER").unwrap_or_else(|_| "sqlite".to_string());

    let pool = match driver.to_lowercase().as_str() {
        "postgres" | "postgresql" => {
            DbPool::Postgres(sqlx::PgPool::connect(&db_url).await.expect("Failed to connect to Postgres"))
        }
        "mysql" => {
            DbPool::MySql(sqlx::MySqlPool::connect(&db_url).await.expect("Failed to connect to MySQL"))
        }
        _ => {
            use std::str::FromStr;
            let options = sqlx::sqlite::SqliteConnectOptions::from_str(&db_url)
                .unwrap_or_else(|_| sqlx::sqlite::SqliteConnectOptions::new())
                .create_if_missing(true);
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .connect_with(options)
                .await
                .expect("Failed to connect to SQLite");
            let _ = sqlx::query("PRAGMA foreign_keys = ON;").execute(&pool).await;
            let _ = sqlx::query("PRAGMA journal_mode = WAL;").execute(&pool).await;
            DbPool::Sqlite(pool)
        }
    };

    let root = get_project_root();
    let schema_path = root.join("schema.sql");
    if schema_path.exists() {
        let sql = fs::read_to_string(&schema_path).expect("Failed to read schema.sql");
        let sql = sql
            .lines()
            .filter(|line| {
                let trimmed = line.trim().to_uppercase();
                !trimmed.starts_with("CREATE DATABASE") && !trimmed.starts_with("USE ")
            })
            .collect::<Vec<_>>()
            .join("\n");

        for statement in sql.split(';') {
            let stmt = statement.trim();
            if stmt.is_empty() || stmt.to_uppercase().starts_with("INSERT") {
                continue;
            }
            match &pool {
                DbPool::Sqlite(p) => { let _ = sqlx::query(stmt).execute(p).await; }
                DbPool::Postgres(p) => { let _ = sqlx::query(stmt).execute(p).await; }
                DbPool::MySql(p) => { let _ = sqlx::query(stmt).execute(p).await; }
            }
        }

        let is_empty = match &pool {
            DbPool::Sqlite(p) => {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items").fetch_one(p).await.unwrap_or(0) == 0
            }
            DbPool::Postgres(p) => {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items").fetch_one(p).await.unwrap_or(0) == 0
            }
            DbPool::MySql(p) => {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items").fetch_one(p).await.unwrap_or(0) == 0
            }
        };

        if is_empty {
            for statement in sql.split(';') {
                let stmt = statement.trim();
                if stmt.is_empty() || !stmt.to_uppercase().starts_with("INSERT") {
                    continue;
                }
                match &pool {
                    DbPool::Sqlite(p) => { let _ = sqlx::query(stmt).execute(p).await; }
                    DbPool::Postgres(p) => { let _ = sqlx::query(stmt).execute(p).await; }
                    DbPool::MySql(p) => { let _ = sqlx::query(stmt).execute(p).await; }
                }
            }
        }
    }
    let _ = DB_POOL.set(pool);
}

// ============================================================
// PERFORMANCE: SHARED STATE / CACHES
// ============================================================

static PROJECT_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let mut starts = Vec::new();
    if let Ok(dir) = std::env::current_dir() { starts.push(dir); }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() { starts.push(parent.to_path_buf()); }
    }
    starts.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    for start in starts {
        let mut dir = start;
        loop {
            if dir.join("pages").exists() { return dir; }
            if !dir.pop() { break; }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
});

static STYLE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<style[^>]*>(.*?)</style>").unwrap());
static ELEMENT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<([a-zA-Z][a-zA-Z0-9-]*)(\s[^>]*)?>").unwrap());
static CONDITIONAL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)\{\{\#if\s+([a-zA-Z0-9_-]+)\s*\}\}(.*?)\{\{/if\}\}"#).unwrap());
static PROP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{\s*([a-zA-Z0-9_-]+)\s*\}\}").unwrap());
static CLASS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\bclass\s*=\s*("([^"]*)"|'([^']*)')"#).unwrap());

static SLOT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)<slot(?:\s+name\s*=\s*["']([^"']+)["'])?\s*(?:/>|>(.*?)</slot>|>)"#).unwrap());
static SQL_PARAM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{{1,2}\s*([a-zA-Z0-9_-]+)\s*\}{1,2}").unwrap());

#[derive(Debug)]
struct CompiledTemplate { template: String, css: String }
static TEMPLATE_CACHE: LazyLock<Mutex<HashMap<PathBuf, Arc<CompiledTemplate>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static VLO_DEBUG: LazyLock<bool> = LazyLock::new(|| std::env::var("VLO_DEBUG").is_ok());

macro_rules! vlo_debug {
    ($($arg:tt)*) => { if *VLO_DEBUG { println!($($arg)*); } };
}

// ============================================================
// VLO RENDER STATE
// ============================================================

#[derive(Default)]
struct RenderedPage {
    html: String,
    styles: Vec<String>,
}

impl RenderedPage {
    fn add_style(&mut self, name: &str, css: &str) {
        let marker = format!("/* VLO:{} */", name);
        if self.styles.iter().any(|style| style.contains(&marker)) {
            return;
        }
        self.styles.push(format!("{}\n{}", marker, css));
    }
}

fn get_project_root() -> PathBuf {
    PROJECT_ROOT.clone()
}

// ============================================================
// VLO SERVER BLOCK & API DEFINITIONS
// ============================================================

fn extract_server_block(content: &str) -> Option<String> {
    let start = content.find("<script server>")?;
    let rest = &content[start + 15..];
    let end = rest.find("</script>")?;
    Some(rest[..end].trim().to_string())
}

fn strip_server_block(content: &str) -> String {
    if let Some(start) = content.find("<script server>") {
        if let Some(end) = content[start..].find("</script>") {
            let end = start + end + "</script>".len();
            return format!("{}{}", &content[..start], &content[end..]);
        }
    }
    content.to_string()
}

fn load_api_actions() -> Result<HashMap<String, String>, String> {
    let file = get_project_root().join("pages/api/api.vlo");
    vlo_debug!("🔎 [VLO API] Loading: {}", file.display());
    if !file.exists() {
        return Err(format!("API file not found: {}", file.display()));
    }

    let content = fs::read_to_string(&file).map_err(|e| format!("Could not read {}: {}", file.display(), e))?;
    let block = extract_server_block(&content).ok_or_else(|| format!("No <script server> block found in {}", file.display()))?;
    let clean = block.trim_start_matches('\u{feff}').replace('\u{a0}', " ").replace('\r', "");
    let json: Value = serde_json::from_str(&clean).map_err(|e| format!("Invalid JSON in {}: {}", file.display(), e))?;
    let object = json.as_object().ok_or_else(|| "API definitions must be a JSON object".to_string())?;

    let mut actions = HashMap::new();
    for (name, value) in object {
        if let Some(sql) = value.as_str() {
            actions.insert(name.clone(), sql.to_string());
        }
    }
    vlo_debug!("✅ [VLO API] Loaded {} actions", actions.len());
    Ok(actions)
}

// ============================================================
// DEVELOPMENT SERVER
// ============================================================

async fn dev(host: &str, port: u16) {
    let root = get_project_root();
    let pages_path = root.join("pages");
    let public_path = root.join("public");
    let public_path_service = public_path.clone();
    let (tx, _) = broadcast::channel::<()>(16);
    let tx_watcher = tx.clone();
    let last_reload = Arc::new(Mutex::new(Instant::now()));

    std::thread::spawn(move || {
        let _ = watch_files(pages_path, public_path, tx_watcher, last_reload);
    });

    let app = Router::new()
        .route("/", get(home_handler))
        .route("/:path", get(page_handler))
        .route("/api", get(api_handler_root).post(api_handler_root).put(api_handler_root).patch(api_handler_root).delete(api_handler_root))
        .route("/api/:resource", get(api_handler_path).post(api_handler_path).put(api_handler_path).patch(api_handler_path).delete(api_handler_path))
        .route("/api/:resource/:id", get(api_handler_id).post(api_handler_id).put(api_handler_id).patch(api_handler_id).delete(api_handler_id))
        .route("/__vlo_hmr", get(move || hmr_handler(tx)))
        .nest_service("/static", ServeDir::new(public_path_service))
        .fallback(not_found_handler);

    let host_str = std::env::var("VLO_HOST").unwrap_or_else(|_| host.to_string());
    let port_str = std::env::var("VLO_PORT").unwrap_or_else(|_| port.to_string());
    let addr = format!("{}:{}", host_str, port_str);

    let listener = tokio::net::TcpListener::bind(&addr).await.expect("Failed to bind port");
    println!("⚡ VLO dev server running at http://{}", addr);
    println!("📁 Project root: {}", root.display());

    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await.expect("Server error");
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
    println!("\n⚡ Shutting down VLO dev server...");
}

// ============================================================
// HOT MODULE RELOAD
// ============================================================

async fn hmr_handler(tx: broadcast::Sender<()>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = tx.subscribe();
    let stream = stream! {
        while rx.recv().await.is_ok() {
            yield Ok(Event::default().data("reload"));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn watch_files(pages: PathBuf, public: PathBuf, tx: broadcast::Sender<()>, last: Arc<Mutex<Instant>>) -> notify::Result<()> {
    let root = get_project_root();
    let layouts = root.join("layouts");
    let components = root.join("components");

    let (tx_notify, rx) = channel();
    let mut watcher = RecommendedWatcher::new(tx_notify, Config::default())?;
    if pages.exists() { watcher.watch(&pages, RecursiveMode::Recursive)?; }
    if public.exists() { watcher.watch(&public, RecursiveMode::Recursive)?; }
    if layouts.exists() { watcher.watch(&layouts, RecursiveMode::Recursive)?; }
    if components.exists() { watcher.watch(&components, RecursiveMode::Recursive)?; }

    for result in rx {
        let Ok(event) = result else { continue; };
        let relevant = event.paths.iter().any(|path| {
            path.extension().and_then(|e| e.to_str()) == Some("vlo")
                || path.starts_with(&public)
                || path.starts_with(&layouts)
                || path.starts_with(&components)
        });
        if !relevant { continue; }

        if let Ok(mut timestamp) = last.try_lock() {
            if timestamp.elapsed().as_millis() > 200 {
                *timestamp = Instant::now();
                if let Ok(mut cache) = TEMPLATE_CACHE.lock() { cache.clear(); }
                let _ = tx.send(());
                println!("⚡ Reload");
            }
        }
    }
    Ok(())
}

// ============================================================
// PRODUCTION BUILD & DEPLOYMENT
// ============================================================

fn build() {
    println!("⚡ Building production site...");
    let root = get_project_root();
    let pages = root.join("pages");
    let public = root.join("public");
    let dist = root.join("dist");

    if dist.exists() { let _ = fs::remove_dir_all(&dist); }
    fs::create_dir_all(dist.join("static")).expect("Failed to create dist directory");

    if let Ok(entries) = fs::read_dir(&pages) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("vlo") { continue; }
            let stem = path.file_stem().unwrap().to_string_lossy().to_string();
            let content = fs::read_to_string(&path).unwrap_or_default();
            let rendered = render_vlo(content);
            let html = wrap_html(&stem, &rendered, false);
            let output = if stem == "home" || stem == "index" { dist.join("index.html") } else { dist.join(format!("{}.html", stem)) };
            fs::write(&output, html).expect("Failed to write generated HTML");
            println!("  ├─ Generated: {}", output.display());
        }
    }
    if public.exists() {
        copy_dir_all(&public, &dist.join("static")).expect("Failed to copy static assets");
        println!("  └─ Copied static assets");
    }
    println!("⚡ Build completed successfully!");
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() { copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?; }
        else { fs::copy(entry.path(), dst.join(entry.file_name()))?; }
    }
    Ok(())
}

async fn deploy(provider: &str) {
    let root = get_project_root();
    let dist = root.join("dist");
    if !dist.exists() { build(); }
    let provider = provider.to_lowercase();
    println!("⚡ Deploying /dist to {}...", provider);

    if provider == "railway" {
        let caddy = dist.join("Caddyfile");
        if !caddy.exists() {
            fs::write(&caddy, ":$PORT {\n    root * .\n    file_server\n}\n").expect("Failed to write Caddyfile");
        }
    }

    let args: Vec<&str> = match provider.as_str() {
        "netlify" => vec!["netlify-cli", "deploy", "--dir=dist", "--prod"],
        "vercel" => vec!["vercel", "deploy", "dist", "--prod"],
        "cloudflare" | "pages" => vec!["wrangler", "pages", "deploy", "dist"],
        "railway" => vec!["@railway/cli", "up"],
        _ => { eprintln!("❌ Unsupported provider '{}'.", provider); return; }
    };

    let working_dir = if provider == "railway" { &dist } else { &root };
    let status = if cfg!(target_os = "windows") {
        Command::new("cmd").arg("/C").arg("npx.cmd").arg("-y").args(&args).current_dir(working_dir).status()
    } else {
        Command::new("npx").arg("-y").args(&args).current_dir(working_dir).status()
    };

    match status {
        Ok(status) if status.success() => println!("⚡ Deployment completed successfully!"),
        Ok(status) => eprintln!("❌ Deployment exited with status: {}", status),
        Err(error) => eprintln!("❌ Failed to execute deployment command: {}", error),
    }
}

// ============================================================
// API ROUTES & HELPERS
// ============================================================

async fn api_handler_root(method: Method, Query(query): Query<HashMap<String, String>>, payload: Option<Json<Value>>) -> impl IntoResponse {
    api_route_handler(None, None, method, query, payload).await
}

async fn api_handler_path(AxumPath(resource): AxumPath<String>, method: Method, Query(query): Query<HashMap<String, String>>, payload: Option<Json<Value>>) -> impl IntoResponse {
    api_route_handler(Some(resource), None, method, query, payload).await
}

async fn api_handler_id(AxumPath((resource, id)): AxumPath<(String, String)>, method: Method, Query(query): Query<HashMap<String, String>>, payload: Option<Json<Value>>) -> impl IntoResponse {
    api_route_handler(Some(resource), Some(id), method, query, payload).await
}

fn crud_operation(method: &Method) -> Option<&'static str> {
    match *method {
        Method::GET => Some("get"),
        Method::POST => Some("post"),
        Method::PUT => Some("put"),
        Method::PATCH => Some("patch"),
        Method::DELETE => Some("delete"),
        _ => None,
    }
}

fn normalize_resource(endpoint: &str) -> String {
    let value = endpoint.trim().trim_matches('/');
    for prefix in ["get_", "post_", "put_", "patch_", "delete_"] {
        if let Some(rest) = value.strip_prefix(prefix) { return rest.to_string(); }
    }
    value.to_string()
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ============================================================
// API REQUEST HANDLER
// ============================================================

async fn api_route_handler(
    mut endpoint: Option<String>, id: Option<String>, method: Method, mut query: HashMap<String, String>, payload: Option<Json<Value>>,
) -> impl IntoResponse {
    if let Some(Json(body)) = payload {
        if let Value::Object(map) = body {
            for (key, value) in map { query.insert(key, value_to_query_string(&value)); }
        }
    }

    if endpoint.is_none() {
        if let Some(action) = query.get("action").cloned() { endpoint = Some(action); }
    }

    let endpoint = match endpoint {
        Some(value) => value.trim().trim_matches('/').to_string(),
        None => return match load_api_actions() {
            Ok(actions) => (StatusCode::OK, Json(serde_json::json!({ "success": true, "actions": actions }))).into_response(),
            Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "success": false, "error": "Failed to load API definitions", "details": error }))).into_response(),
        },
    };

    let resource = normalize_resource(&endpoint);
    if !valid_identifier(&resource) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "success": false, "error": "Invalid API resource", "resource": resource }))).into_response();
    }

    let operation = match crud_operation(&method) {
        Some(value) => value,
        None => return (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({ "success": false, "error": "Unsupported HTTP method" }))).into_response(),
    };

    let explicit_action = ["get_", "post_", "put_", "patch_", "delete_"].iter().any(|prefix| endpoint.starts_with(prefix));
    let action_name = if explicit_action { endpoint.clone() } else { format!("{}_{}", operation, resource) };

    vlo_debug!("🔎 [VLO API] {} /api/{}{} -> {}", method, resource, id.as_ref().map(|v| format!("/{}", v)).unwrap_or_default(), action_name);

    let actions = match load_api_actions() {
        Ok(value) => value,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "success": false, "error": "Failed to load API definitions", "details": error }))).into_response(),
    };

    let mut sql = match actions.get(&action_name) {
        Some(value) => value.clone(),
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "success": false, "error": "API operation not found", "action": action_name }))).into_response(),
    };

    if let Some(id_value) = id.clone() { query.insert("id".to_string(), id_value); }
    let mut params = serde_json::Map::new();
    for (key, value) in query { if key != "action" { params.insert(key, query_string_to_value(&value)); } }

    if id.is_some() && operation == "get" && !sql.contains("{{id}}") && !sql.contains("{id}") {
        sql = add_id_filter(&sql, &params);
    }

    let pool = match DB_POOL.get() {
        Some(p) => p,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "success": false, "error": "Database not configured" }))).into_response(),
    };

    match execute_api_sql(pool, &sql, &params).await {
        Ok(data) => (StatusCode::OK, Json(data)).into_response(),
        Err(error) => {
            eprintln!("❌ [VLO API] SQL error in {}: {}", action_name, error);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "success": false, "error": "SQL Execution Error", "details": error, "action": action_name, "sql": sql }))).into_response()
        }
    }
}

// ============================================================
// API / SQL VALUES & PREPARATION
// ============================================================

enum QueryParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Json(serde_json::Value),
}

fn value_to_any_param(value: &Value) -> QueryParam {
    match value {
        Value::Null => QueryParam::Null,
        Value::Bool(b) => QueryParam::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() { QueryParam::Int(i) }
            else if let Some(f) = n.as_f64() { QueryParam::Float(f) }
            else { QueryParam::Text(n.to_string()) }
        }
        Value::String(s) => QueryParam::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => QueryParam::Json(value.clone()),
    }
}

fn prepare_sql(sql: &str, params: &serde_json::Map<String, Value>, pool: &DbPool) -> Result<(String, Vec<QueryParam>), String> {
    let mut values = Vec::new();
    let mut param_index = 1;
    let is_postgres = matches!(pool, DbPool::Postgres(_));

    let prepared = SQL_PARAM_RE.replace_all(sql, |caps: &regex::Captures| {
        let key = &caps[1];
        match params.get(key) {
            Some(value) => {
                let placeholder = if is_postgres {
                    let ph = format!("${}", param_index);
                    param_index += 1;
                    ph
                } else {
                    param_index += 1;
                    "?".to_string()
                };
                values.push(value_to_any_param(value));
                placeholder
            }
            None => {
                let placeholder = if is_postgres {
                    let ph = format!("${}", param_index);
                    param_index += 1;
                    ph
                } else {
                    param_index += 1;
                    "?".to_string()
                };
                values.push(QueryParam::Null);
                placeholder
            }
        }
    }).into_owned();

    if prepared.contains('{') || prepared.contains('}') {
        return Err(format!("Unresolved parameter in SQL: {}", prepared));
    }
    Ok((prepared, values))
}

fn add_id_filter(sql: &str, _params: &serde_json::Map<String, Value>) -> String {
    let upper = sql.to_uppercase();
    if let Some(pos) = upper.find(" ORDER BY ") {
        let before = sql[..pos].trim_end();
        let order = &sql[pos..];
        if before.to_uppercase().contains(" WHERE ") { format!("{} AND id = {{id}}{}", before, order) }
        else { format!("{} WHERE id = {{id}}{}", before, order) }
    } else {
        let trimmed = sql.trim_end_matches(';').trim_end();
        if trimmed.to_uppercase().contains(" WHERE ") { format!("{} AND id = {{id}}", trimmed) }
        else { format!("{} WHERE id = {{id}}", trimmed) }
    }
}

// ============================================================
// DYNAMIC ARGUMENT BUILDERS
// ============================================================

fn build_sqlite_args(params: &[QueryParam]) -> sqlx::sqlite::SqliteArguments<'static> {
    let mut args = sqlx::sqlite::SqliteArguments::default();
    for p in params {
        match p {
            QueryParam::Null => { let _ = args.add(Option::<String>::None); }
            QueryParam::Bool(b) => { let _ = args.add(*b); }
            QueryParam::Int(i) => { let _ = args.add(*i); }
            QueryParam::Float(f) => { let _ = args.add(*f); }
            QueryParam::Text(s) => { let _ = args.add(s.clone()); }
            QueryParam::Json(j) => { let _ = args.add(sqlx::types::Json(j.clone())); }
        }
    }
    args
}

fn build_pg_args(params: &[QueryParam]) -> sqlx::postgres::PgArguments {
    let mut args = sqlx::postgres::PgArguments::default();
    for p in params {
        match p {
            QueryParam::Null => { let _ = args.add(Option::<String>::None); }
            QueryParam::Bool(b) => { let _ = args.add(*b); }
            QueryParam::Int(i) => { let _ = args.add(*i); }
            QueryParam::Float(f) => { let _ = args.add(*f); }
            QueryParam::Text(s) => { let _ = args.add(s.clone()); }
            QueryParam::Json(j) => { let _ = args.add(sqlx::types::Json(j.clone())); }
        }
    }
    args
}

fn build_mysql_args(params: &[QueryParam]) -> sqlx::mysql::MySqlArguments {
    let mut args = sqlx::mysql::MySqlArguments::default();
    for p in params {
        match p {
            QueryParam::Null => { let _ = args.add(Option::<String>::None); }
            QueryParam::Bool(b) => { let _ = args.add(*b); }
            QueryParam::Int(i) => { let _ = args.add(*i); }
            QueryParam::Float(f) => { let _ = args.add(*f); }
            QueryParam::Text(s) => { let _ = args.add(s.clone()); }
            QueryParam::Json(j) => { let _ = args.add(sqlx::types::Json(j.clone())); }
        }
    }
    args
}

// ============================================================
// GENERIC ROW TO JSON CONVERTERS (SQLite, Postgres, MySQL)
// ============================================================

macro_rules! convert_row_to_json {
    ($row:expr) => {{
        let mut map = serde_json::Map::new();
        for (i, column) in $row.columns().iter().enumerate() {
            let name = column.name().to_string();
            let is_null = $row.try_get_raw(i).map(|r| r.is_null()).unwrap_or(true);

            let val: serde_json::Value = if is_null {
                serde_json::Value::Null
            } else {
                let type_name = column.type_info().name().to_lowercase();
                if type_name.contains("int") {
                    $row.try_get::<i64, _>(i)
                        .or_else(|_| $row.try_get::<i32, _>(i).map(|v| v as i64))
                        .or_else(|_| $row.try_get::<i16, _>(i).map(|v| v as i64))
                        .map(|v| serde_json::Value::Number(v.into()))
                        .unwrap_or(serde_json::Value::Null)
                } else if type_name.contains("bool") {
                    $row.try_get::<bool, _>(i).map(serde_json::Value::Bool).unwrap_or(serde_json::Value::Null)
                } else if type_name.contains("float") || type_name.contains("double") || type_name.contains("real") || type_name.contains("numeric") || type_name.contains("decimal") {
                    $row.try_get::<f64, _>(i)
                        .or_else(|_| $row.try_get::<f32, _>(i).map(|v| v as f64))
                        .ok()
                        .and_then(|v| serde_json::Number::from_f64(v).map(serde_json::Value::Number))
                        .unwrap_or(serde_json::Value::Null)
                } else if type_name.contains("json") {
                    $row.try_get::<sqlx::types::Json<serde_json::Value>, _>(i)
                        .map(|j| j.0)
                        .or_else(|_| $row.try_get::<String, _>(i).ok().and_then(|s| serde_json::from_str(&s).ok()).ok_or(sqlx::Error::RowNotFound))
                        .unwrap_or(serde_json::Value::Null)
                } else {
                    $row.try_get::<String, _>(i).map(serde_json::Value::String).unwrap_or_else(|_| {
                        $row.try_get::<Vec<u8>, _>(i).map(|v| serde_json::Value::String(format!("blob {}b", v.len()))).unwrap_or(serde_json::Value::Null)
                    })
                }
            };
            map.insert(name, val);
        }
        serde_json::Value::Object(map)
    }};
}

fn sqlite_row_to_json(row: &sqlx::sqlite::SqliteRow) -> serde_json::Value { convert_row_to_json!(row) }
fn pg_row_to_json(row: &sqlx::postgres::PgRow) -> serde_json::Value { convert_row_to_json!(row) }
fn mysql_row_to_json(row: &sqlx::mysql::MySqlRow) -> serde_json::Value { convert_row_to_json!(row) }

// ============================================================
// API / SQL EXECUTION
// ============================================================

async fn execute_api_sql(pool: &DbPool, sql: &str, params: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let mut last_data = None;
    let mut affected_rows = 0u64;

    macro_rules! exec_db {
        ($pool:expr, $builder:ident, $to_json:ident) => {{
            let mut tx = $pool.begin().await.map_err(|e| e.to_string())?;
            for statement in sql.split(';') {
                let statement = statement.trim();
                if statement.is_empty() { continue; }

                let (prepared_sql, values) = prepare_sql(statement, params, pool)?;
                let upper = prepared_sql.trim_start().to_uppercase();
                let is_select = upper.starts_with("SELECT") || upper.starts_with("PRAGMA") || upper.starts_with("WITH");

                if is_select {
                    let args = $builder(&values);
                    let rows = sqlx::query_with(&prepared_sql, args).fetch_all(&mut *tx).await.map_err(|e| e.to_string())?;
                    let mut data = Vec::new();
                    for row in rows { data.push($to_json(&row)); }
                    last_data = Some(data);
                } else {
                    let args = $builder(&values);
                    let res = sqlx::query_with(&prepared_sql, args).execute(&mut *tx).await.map_err(|e| e.to_string())?;
                    affected_rows += res.rows_affected();
                }
            }
            tx.commit().await.map_err(|e| e.to_string())?;
        }};
    }

    match pool {
        DbPool::Sqlite(p) => exec_db!(p, build_sqlite_args, sqlite_row_to_json),
        DbPool::Postgres(p) => exec_db!(p, build_pg_args, pg_row_to_json),
        DbPool::MySql(p) => exec_db!(p, build_mysql_args, mysql_row_to_json),
    }

    if let Some(data) = last_data {
        Ok(serde_json::json!({ "data": data, "affected_rows": affected_rows }))
    } else {
        Ok(serde_json::json!({ "success": true, "affected_rows": affected_rows }))
    }
}

fn value_to_query_string(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        Value::Number(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

fn query_string_to_value(value: &str) -> Value {
    if value.is_empty() { return Value::String(value.to_string()); }
    if value.eq_ignore_ascii_case("true") { return Value::Bool(true); }
    if value.eq_ignore_ascii_case("false") { return Value::Bool(false); }
    if let Ok(integer) = value.parse::<i64>() { return Value::Number(integer.into()); }
    if let Ok(float) = value.parse::<f64>() {
        if let Some(number) = serde_json::Number::from_f64(float) { return Value::Number(number); }
    }
    Value::String(value.to_string())
}

// ============================================================
// PAGE ROUTES & RENDERING
// ============================================================

async fn home_handler() -> impl IntoResponse { render_page("home".to_string(), true).await }
async fn page_handler(AxumPath(path): AxumPath<String>) -> impl IntoResponse { render_page(path, true).await }
async fn not_found_handler() -> impl IntoResponse { render_404(true).await }

async fn render_page(path: String, dev: bool) -> impl IntoResponse {
    let page_path = path.clone();
    match tokio::task::spawn_blocking(move || {
        let file = get_project_root().join("pages").join(format!("{}.vlo", page_path));
        fs::read_to_string(file).ok().map(|content| {
            let rendered = render_vlo(content);
            (StatusCode::OK, Html(wrap_html(&page_path, &rendered, dev)))
        })
    }).await {
        Ok(Some(response)) => response.into_response(),
        _ => render_404(dev).await.into_response(),
    }
}

async fn render_404(dev: bool) -> impl IntoResponse {
    tokio::task::spawn_blocking(move || {
        let file = get_project_root().join("pages").join("404.vlo");
        if let Ok(content) = fs::read_to_string(file) {
            let rendered = render_vlo(content);
            (StatusCode::NOT_FOUND, Html(wrap_html("404 - Page Not Found", &rendered, dev)))
        } else {
            let fallback = r#"<div class="not-found"><h1>404</h1><p>Page Not Found</p><a href="/">Back to Home</a></div>"#;
            let rendered = RenderedPage { html: fallback.to_string(), ..Default::default() };
            (StatusCode::NOT_FOUND, Html(wrap_html("404 - Page Not Found", &rendered, dev)))
        }
    }).await.unwrap_or((StatusCode::INTERNAL_SERVER_ERROR, Html("Server Error".to_string())))
}

fn strip_blank_lines(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    for line in html.lines() { if !line.trim().is_empty() { out.push_str(line); out.push('\n'); } }
    out
}

fn render_vlo(source: String) -> RenderedPage {
    let mut context = RenderedPage::default();
    let mut source = strip_server_block(&source);

    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) {
            context.add_style("page", style.as_str());
        }
    }
    source = STYLE_RE.replace_all(&source, "").into_owned();

    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, "BaseLayout", &mut context);
        source = render_components(&source, &mut context);
        if source == previous {
            break;
        }
    }

    context.html = strip_blank_lines(&source);
    context
}

fn render_tag(source: &str, tag: &str, context: &mut RenderedPage) -> String {
    if let Some((start, end, props, children)) = find_tag(source, tag) {
        return format!("{}{}{}", &source[..start], render_component_file(tag, &props, &children, context), &source[end..]);
    }
    source.to_string()
}

fn render_components(source: &str, context: &mut RenderedPage) -> String {
    let mut output = String::new();
    let mut last = 0;
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut index = 0;

    while index < chars.len() {
        let (position, character) = chars[index];
        if character == '<' && index + 1 < chars.len() && chars[index + 1].1.is_ascii_uppercase() {
            let mut end = index + 1;
            while end < chars.len() && (chars[end].1.is_ascii_alphanumeric() || chars[end].1 == '_') { end += 1; }
            let tag = &source[chars[index + 1].0..chars[end].0];
            if let Some((_, tag_end, props, children)) = find_tag(&source[position..], tag) {
                output.push_str(&source[last..position]);
                output.push_str(&render_component_file(tag, &props, &children, context));
                last = position + tag_end;
                while index < chars.len() && chars[index].0 < last { index += 1; }
                continue;
            }
        }
        index += 1;
    }
    output.push_str(&source[last..]);
    output
}

fn component_path(name: &str) -> Option<PathBuf> {
    let root = get_project_root();
    let layout = root.join("layouts").join(format!("{}.vlo", name));
    if layout.exists() { return Some(layout); }
    let component = root.join("components").join(format!("{}.vlo", name));
    if component.exists() { return Some(component); }
    None
}

fn read_component_template(path: &Path) -> Option<Arc<CompiledTemplate>> {
    if let Ok(cache) = TEMPLATE_CACHE.lock() {
        if let Some(cached) = cache.get(path) { return Some(Arc::clone(cached)); }
    }
    let source = fs::read_to_string(path).ok()?;
    let mut css = String::new();
    for captures in STYLE_RE.captures_iter(&source) {
        if let Some(style) = captures.get(1) { css.push_str(style.as_str()); css.push('\n'); }
    }
    let template = STYLE_RE.replace_all(&source, "").into_owned();
    let compiled = Arc::new(CompiledTemplate { template, css });
    if let Ok(mut cache) = TEMPLATE_CACHE.lock() { cache.insert(path.to_path_buf(), Arc::clone(&compiled)); }
    Some(compiled)
}

fn find_tag(source: &str, name: &str) -> Option<(usize, usize, String, String)> {
    let open = format!("<{}", name);
    let start = source.find(&open)?;
    let mut index = start + open.len();
    let next = source[index..].chars().next()?;
    if !(next.is_whitespace() || next == '/' || next == '>') { return None; }
    let props_start = index;
    let mut quote = None;
    let mut open_end = None;

    for (offset, character) in source[index..].char_indices() {
        match quote {
            Some(current) if character == current => { quote = None; }
            None if character == '"' || character == '\'' => { quote = Some(character); }
            None if character == '>' => { open_end = Some(index + offset); break; }
            _ => {}
        }
    }
    let open_end = open_end?;
    let props = source[props_start..open_end].to_string();
    let self_closing = props.trim_end().ends_with('/');
    index = open_end + 1;
    if self_closing { return Some((start, index, props, String::new())); }
    let close = format!("</{}>", name);
    let children_start = index;
    let mut depth = 1;

    while index < source.len() {
        let remaining = &source[index..];
        if remaining.starts_with(&open) {
            let after = index + open.len();
            let valid = source[after..].chars().next().map(|c| c.is_whitespace() || c == '/' || c == '>').unwrap_or(false);
            if valid { depth += 1; }
        } else if remaining.starts_with(&close) {
            depth -= 1;
            if depth == 0 { return Some((start, index + close.len(), props, source[children_start..index].to_string())); }
        }
        index += remaining.chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
    }
    None
}

// ============================================================
// COMPONENT FILE RENDERER
// ============================================================

fn render_component_file(name: &str, props_str: &str, children: &str, context: &mut RenderedPage) -> String {
    let path = match component_path(name) {
        Some(path) => path,
        None => return format!("<!-- Missing component: {} -->", name),
    };
    let compiled = match read_component_template(&path) {
        Some(template) => template,
        None => return format!("<!-- Missing template: {} -->", name),
    };
    if !compiled.css.trim().is_empty() { context.add_style(name, compiled.css.trim()); }
    let template = &compiled.template;
    let mut props = parse_props(props_str);
    let (raw_named_slots, raw_default_slot) = parse_slot_content(children);

    vlo_debug!("🧩 [VLO SLOT] Component <{}> received named slots: {:?}", name, raw_named_slots.keys().collect::<Vec<_>>());
    vlo_debug!("🎯 [VLO SLOT] Component <{}> default slot length={}", name, raw_default_slot.len());

    let mut named_slots = HashMap::new();
    for (slot_name, slot_content) in raw_named_slots {
        let rendered = render_nested_vlo_content(&slot_content, context);
        vlo_debug!("🧩 [VLO SLOT] Rendered named slot '{}' length={}", slot_name, rendered.len());
        named_slots.insert(slot_name, rendered);
    }
    let default_slot = render_nested_vlo_content(&raw_default_slot, context);
    vlo_debug!("🎯 [VLO SLOT] Rendered default slot length={}", default_slot.len());
    props.insert("children".to_string(), default_slot.clone());

    let rendered = render_component_template(template, &props);
    let rendered = render_slots(&rendered, &named_slots, &default_slot);

    let incoming_class = props.get("class").cloned();
    let attributes = build_component_attributes(template, &props);

    if attributes.is_empty() && incoming_class.is_none() {
        return rendered;
    }

    let skip_check = STYLE_RE.replace_all(&rendered, "");
    if let Some(first_tag) = ELEMENT_RE.captures(&skip_check).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()) {
        if first_tag.eq_ignore_ascii_case("style") || first_tag.eq_ignore_ascii_case("script") {
            return rendered;
        }
    }

    if let Some(captures) = ELEMENT_RE.captures(&rendered) {
        let full_match = captures.get(0).unwrap();
        let tag_name = captures.get(1).unwrap().as_str();
        let existing_attributes = captures.get(2).map(|v| v.as_str()).unwrap_or("").to_string();

        let (existing_attributes, class_attr) = if let Some(extra) = incoming_class.as_deref().map(|c| c.trim()).filter(|c| !c.is_empty()) {
            if let Some(m) = CLASS_RE.captures(&existing_attributes) {
                let existing_value = m.get(2).or_else(|| m.get(3)).map(|v| v.as_str()).unwrap_or("").trim();
                let merged = if existing_value.is_empty() {
                    format!("class=\"{}\"", extra)
                } else {
                    format!("class=\"{} {}\"", existing_value, extra)
                };
                let stripped = CLASS_RE.replace(&existing_attributes, "").trim().to_string();
                (stripped, Some(merged))
            } else {
                (existing_attributes, Some(format!("class=\"{}\"", extra)))
            }
        } else {
            (existing_attributes, None)
        };

        let attributes = match class_attr {
            Some(c) if attributes.is_empty() => c,
            Some(c) => format!("{} {}", c, attributes),
            None => attributes,
        };

        if attributes.is_empty() {
            return rendered;
        }

        let replacement = if existing_attributes.trim().is_empty() {
            format!("<{} {}>", tag_name, attributes)
        } else {
            format!("<{} {} {}>", tag_name, existing_attributes.trim(), attributes)
        };
        return format!("{}{}{}", &rendered[..full_match.start()], replacement, &rendered[full_match.end()..]);
    }
    rendered
}

fn render_nested_vlo_content(content: &str, context: &mut RenderedPage) -> String {
    if content.trim().is_empty() { return String::new(); }
    let mut source = strip_server_block(content);
    for _ in 0..20 {
        let previous = source.clone();
        source = render_tag(&source, "BaseLayout", context);
        source = render_components(&source, context);
        if source == previous { break; }
    }
    source
}

fn render_component_template(template: &str, props: &HashMap<String, String>) -> String {
    let button_default = template.to_ascii_lowercase().contains("<button");
    let mut rendered = CONDITIONAL_RE.replace_all(template, |captures: &regex::Captures| {
        let key = captures[1].trim();
        let value = props.get(key).map(|v| v.trim()).or_else(|| {
            if button_default && key == "type" && !props.contains_key("type") { Some("button") } else { None }
        }).unwrap_or("");
        if value.is_empty() || value.eq_ignore_ascii_case("false") { String::new() } else { captures[2].to_string() }
    }).into_owned();

    rendered = PROP_RE.replace_all(&rendered, |captures: &regex::Captures| {
        let key = captures[1].trim();
        if let Some(value) = props.get(key) { value.clone() }
        else if button_default && key == "type" { "button".to_string() }
        else { String::new() }
    }).into_owned();

    normalize_class_attributes(&rendered)
}

fn normalize_class_attributes(html: &str) -> String {
    CLASS_RE.replace_all(html, |captures: &regex::Captures| {
        let value = captures.get(2).or_else(|| captures.get(3)).map(|v| v.as_str()).unwrap_or("");
        let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
        let quote = if captures.get(1).map(|v| v.as_str()).unwrap_or("").starts_with('"') { '"' } else { '\'' };
        format!("class={quote}{normalized}{quote}")
    }).into_owned()
}

fn build_component_attributes(template: &str, props: &HashMap<String, String>) -> String {
    let mut used = HashSet::new();
    for captures in PROP_RE.captures_iter(template) { used.insert(captures[1].to_string()); }
    let mut attributes = Vec::new();
    let button_default = template.to_ascii_lowercase().contains("<button");

    if button_default && !props.contains_key("type") && !used.contains("type") { attributes.push("type=\"button\"".to_string()); }
    for (key, value) in props {
        if key == "children" || key == "attributes" || key == "class" { continue; }
        if used.contains(key) { continue; }
        if is_boolean_attribute(key) {
            if value.eq_ignore_ascii_case("true") || value == key { attributes.push(key.clone()); }
            continue;
        }
        if value.trim().is_empty() { continue; }
        attributes.push(format!("{}=\"{}\"", key, escape_html_attribute(value)));
    }
    attributes.sort();
    attributes.join(" ")
}

fn is_boolean_attribute(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "allowfullscreen" | "async" | "autofocus" | "autoplay" | "checked" | "controls" | "default" | "defer" | "disabled" | "formnovalidate" | "hidden" | "inert" | "ismap" | "itemscope" | "loop" | "multiple" | "muted" | "nomodule" | "novalidate" | "open" | "playsinline" | "readonly" | "required" | "reversed" | "selected")
}

fn escape_html_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;").replace('\'', "&#39;").replace('<', "&lt;").replace('>', "&gt;")
}

fn parse_props(raw: &str) -> HashMap<String, String> {
    let chars: Vec<char> = raw.replace("&quot;", "\"").replace("&#39;", "'").chars().collect();
    let mut map = HashMap::new();
    let mut index = 0;

    while index < chars.len() {
        while index < chars.len() && chars[index].is_whitespace() { index += 1; }
        if index >= chars.len() || chars[index] == '/' { break; }
        let mut key = String::new();
        while index < chars.len() && chars[index] != '=' && !chars[index].is_whitespace() && chars[index] != '/' {
            key.push(chars[index]); index += 1;
        }
        if key.is_empty() { index += 1; continue; }
        while index < chars.len() && chars[index].is_whitespace() { index += 1; }
        if index < chars.len() && chars[index] == '=' {
            index += 1;
            while index < chars.len() && chars[index].is_whitespace() { index += 1; }
            if index >= chars.len() { map.insert(key, "true".to_string()); break; }
            let quote = chars[index];
            let mut value = String::new();
            if quote == '"' || quote == '\'' {
                index += 1;
                while index < chars.len() && chars[index] != quote { value.push(chars[index]); index += 1; }
                if index < chars.len() { index += 1; }
            } else {
                while index < chars.len() && !chars[index].is_whitespace() && chars[index] != '/' { value.push(chars[index]); index += 1; }
            }
            map.insert(key, value);
        } else {
            map.insert(key, "true".to_string());
        }
    }
    map
}

fn render_slots(template: &str, named_slots: &HashMap<String, String>, default_slot: &str) -> String {
    vlo_debug!("🎯 [VLO SLOT] Rendering slots: {:?}, default_len={}", named_slots.keys().collect::<Vec<_>>(), default_slot.len());
    let res = SLOT_RE.replace_all(template, |captures: &regex::Captures| {
        let name = captures.get(1).map(|v| v.as_str().trim()).unwrap_or("");
        let fallback = captures.get(2).map(|v| v.as_str()).unwrap_or("");
        vlo_debug!("🔎 [VLO SLOT] Template slot requested: '{}'", name);
        if name.is_empty() {
            if default_slot.trim().is_empty() { fallback.to_string() } else { default_slot.to_string() }
        } else if let Some(content) = named_slots.get(name) {
            content.clone()
        } else {
            fallback.to_string()
        }
    }).into_owned();

    SLOT_RE.replace_all(&res, "").into_owned()
}

fn parse_slot_content(children: &str) -> (HashMap<String, String>, String) {
    let mut named_slots = HashMap::new();
    let mut default_content = String::new();
    let mut cursor = 0;
    let mut default_start = 0;

    while cursor < children.len() {
        let remaining = &children[cursor..];
        let open_start = match remaining.find('<') {
            Some(offset) => cursor + offset,
            None => break,
        };
        let tag_info = match parse_element_at(children, open_start) {
            Some(info) => info,
            None => { cursor = open_start + 1; continue; }
        };
        let (tag_name, _opening_end, element_end, props, content) = tag_info;
        if let Some(slot_name) = get_slot_name(&props) {
            if open_start > default_start { default_content.push_str(&children[default_start..open_start]); }
            vlo_debug!("🧩 [VLO SLOT] Found named slot '{}' in <{}>", slot_name, tag_name);
            named_slots.entry(slot_name).or_insert_with(String::new).push_str(content);
            cursor = element_end;
            default_start = cursor;
            continue;
        }
        cursor = element_end;
    }
    if default_start < children.len() { default_content.push_str(&children[default_start..]); }
    (named_slots, default_content)
}

fn get_slot_name(props: &str) -> Option<String> {
    parse_props(props).get("slot").cloned()
}

fn is_void_tag(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}

fn parse_element_at(source: &str, start: usize) -> Option<(String, usize, usize, String, &str)> {
    if !source[start..].starts_with('<') { return None; }
    let mut cursor = start + 1;
    if cursor >= source.len() { return None; }
    let first = source[cursor..].chars().next()?;
    if first == '/' || first == '!' || first == '?' { return None; }
    let tag_start = cursor;
    while cursor < source.len() {
        let ch = source[cursor..].chars().next()?;
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == ':' { cursor += ch.len_utf8(); }
        else { break; }
    }
    if cursor == tag_start { return None; }
    let tag_name = source[tag_start..cursor].to_string();
    let opening_end = find_tag_opening_end(source, cursor)?;
    let props = source[cursor..opening_end].to_string();
    let self_closing = props.trim_end().ends_with('/') || is_void_tag(&tag_name);
    if self_closing { return Some((tag_name, opening_end + 1, opening_end + 1, props, "")); }
    let content_start = opening_end + 1;
    let element_end = find_matching_tag_end(source, content_start, &tag_name)?;
    let close_start = element_end.checked_sub(format!("</{}>", tag_name).len())?;
    if close_start < content_start { return None; }
    let content = &source[content_start..close_start];
    Some((tag_name, opening_end + 1, element_end, props, content))
}

fn find_tag_opening_end(source: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    let mut cursor = start;
    while cursor < source.len() {
        let ch = source[cursor..].chars().next()?;
        match quote {
            Some(active) => { if ch == active { quote = None; } }
            None => {
                if ch == '"' || ch == '\'' { quote = Some(ch); }
                else if ch == '>' { return Some(cursor); }
            }
        }
        cursor += ch.len_utf8();
    }
    None
}

fn find_matching_tag_end(source: &str, start: usize, tag_name: &str) -> Option<usize> {
    let opening = format!("<{}", tag_name);
    let closing = format!("</{}>", tag_name);
    let mut depth = 1;
    let mut cursor = start;

    while cursor < source.len() {
        let remaining = &source[cursor..];
        if remaining.starts_with(&closing) {
            depth -= 1;
            if depth == 0 { return Some(cursor + closing.len()); }
            cursor += closing.len();
            continue;
        }
        if remaining.starts_with(&opening) {
            let after_name = cursor + opening.len();
            let valid = source[after_name..].chars().next().map(|ch| ch.is_whitespace() || ch == '>' || ch == '/').unwrap_or(false);
            if valid {
                let opening_end = find_tag_opening_end(source, after_name)?;
                let props = source[after_name..opening_end].trim();
                if !props.ends_with('/') { depth += 1; }
                cursor = opening_end + 1;
                continue;
            }
        }
        cursor += remaining.chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
    }
    None
}

// ============================================================
// HTML DOCUMENT WRAPPER
// ============================================================

fn wrap_html(title: &str, rendered: &RenderedPage, dev: bool) -> String {
    let hmr = if dev {
        r#"<script>
const es = new EventSource("/__vlo_hmr");
es.onmessage = () => location.reload();
window.addEventListener("beforeunload", () => es.close());
</script>"#
    } else {
        ""
    };

    let component_styles = if rendered.styles.is_empty() {
        String::new()
    } else {
        format!("\n<style>\n{}\n</style>", rendered.styles.join("\n"))
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{}</title>
    <link rel="icon" href="/static/favicon.ico">
    <link rel="stylesheet" href="/static/style.css">{}
</head>
<body>
{}
{}
</body>
</html>"#,
        title, component_styles, rendered.html, hmr
    )
}