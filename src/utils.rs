use std::{
    fs,
    path::{Path, PathBuf},
};

// ============================================================
// 27. VLO INIT / PROJECT BOILERPLATE
// ============================================================

pub fn init_project(
    name: &str,
    db_driver: &str,
    db_name_opt: Option<&str>,
    no_db: bool,
) {
    let target_dir = if name == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(name)
    };

    println!(
        "⚡ Initializing VLO v0.7 project in '{}'...",
        target_dir.display()
    );

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

            let sanitized = base_name.replace(
                |c: char| !c.is_alphanumeric() && c != '_' && c != '-',
                "_",
            );

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

    let env_path = target_dir.join(".env");
    if !env_path.exists() {
        let env_content = if no_db {
            "# Database disabled\n# DB_DRIVER=sqlite\n# DATABASE_URL=sqlite://app.db\n".to_string()
        } else {
            match db_driver.to_lowercase().as_str() {
                "postgres" | "postgresql" => {
                    format!(
                        "DB_DRIVER=postgres\nDATABASE_URL=postgres://postgres:password@localhost:5432/{}\n",
                        db_name
                    )
                }
                "mysql" => {
                    format!(
                        "DB_DRIVER=mysql\nDATABASE_URL=mysql://root:password@localhost:3306/{}\n",
                        db_name
                    )
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

    let schema_path = target_dir.join("schema.sql");
    if !schema_path.exists() && !no_db {
        let schema_sql = match db_driver.to_lowercase().as_str() {
            "postgres" | "postgresql" => {
                r#"CREATE TABLE IF NOT EXISTS items (id SERIAL PRIMARY KEY, title TEXT NOT NULL);
INSERT INTO items (title) VALUES ('⚡ Learn VLO v0.7 Architecture'), ('🛠️ Explore Component Composition'), ('🚀 Build Zero-Boilerplate APIs');"#
            }
            "mysql" => {
                r#"CREATE TABLE IF NOT EXISTS items (id INT AUTO_INCREMENT PRIMARY KEY, title VARCHAR(255) NOT NULL);
INSERT INTO items (title) VALUES ('⚡ Learn VLO v0.7 Architecture'), ('🛠️ Explore Component Composition'), ('🚀 Build Zero-Boilerplate APIs');"#
            }
            _ => {
                r#"CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL);
INSERT INTO items (title) VALUES ('⚡ Learn VLO v0.7 Architecture'), ('🛠️ Explore Component Composition'), ('🚀 Build Zero-Boilerplate APIs');"#
            }
        };

        fs::write(&schema_path, schema_sql).expect("Failed to write schema.sql");
    }

    let api_vlo_path = api_dir.join("api.vlo");
    if !api_vlo_path.exists() {
        fs::write(
            &api_vlo_path,
            r#"<script server>
{
    "get_items": "SELECT * FROM items ORDER BY id DESC;",
    "post_items": "INSERT INTO items (title) VALUES ({{title}});",
    "put_items": "UPDATE items SET title = {{title}} WHERE id = {{id}};",
    "delete_items": "DELETE FROM items WHERE id = {{id}};"
}
</script>"#,
        )
        .expect("Failed to write api.vlo");
    }

    let layout_path = layouts_dir.join("BaseLayout.vlo");
    if !layout_path.exists() {
        fs::write(
            &layout_path,
            r#"<div class="layout-container">
    <header class="app-header"><div class="brand"><h1>⚡ {{title}}</h1></div>
    <nav class="app-nav"><a href="/">Home</a><a href="/about">About</a></nav></header>
    <main class="app-main"><slot></slot></main>
    <footer class="app-footer"><p>Built with ⚡ <strong>VLO v0.7</strong></p></footer>
</div>
<style>
    .layout-container { max-width: 720px; margin: 0 auto; padding: 2.5rem 1.5rem; }
    .app-header { display: flex; justify-content: space-between; align-items: center; border-bottom: 1px solid #334155; padding-bottom: 1.25rem; margin-bottom: 2rem; }
    .app-header h1 { font-size: 1.5rem; font-weight: 700; margin: 0; color: #f8fafc; }
    .app-nav { display: flex; gap: 1.25rem; }
    .app-nav a { color: #94a3b8; text-decoration: none; font-weight: 500; transition: color 0.15s ease; }
    .app-nav a:hover { color: #38bdf8; }
    .app-main { min-height: 320px; }
    .app-footer { border-top: 1px solid #334155; padding-top: 1.5rem; margin-top: 3rem; text-align: center; color: #64748b; font-size: 0.875rem; }
</style>"#,
        )
        .expect("Failed to write BaseLayout.vlo");
    }

    let card_path = components_dir.join("Card.vlo");
    if !card_path.exists() {
        fs::write(
            &card_path,
            r#"<div class="vlo-card">
    {{#if title}}
        <h3>{{title}}</h3>
    {{/#if}}
    {{#if description}}
        <p class="card-desc">{{description}}</p>
    {{/#if}}
    <slot></slot>
</div>
<style>.vlo-card { background: #1e293b; border: 1px solid #334155; border-radius: 12px; padding: 1.5rem; box-shadow: 0 4px 6px -1px rgba(0,0,0,0.3); margin-bottom: 1.5rem; }</style>"#,
        )
        .expect("Failed to write Card.vlo");
    }

    let home_path = pages_dir.join("home.vlo");
    if !home_path.exists() {
        fs::write(
            &home_path,
            r#"<BaseLayout title="VLO v0.7 Dashboard">
    <Card>
        <h2>Items (Zero-JS SSR CRUD)</h2>
        <form action="/api/items" method="POST" class="crud-form">
            <input name="title" placeholder="Add new task..." required/>
            <button type="submit">+ Add</button>
        </form>
        <div data-source="/api/get_items">
            {{#for item in items}}
            <div class="item-row">
                <span>{{item.title}}</span>
                <div class="actions">
                    <button v-put="/api/items/{{item.id}}" v-prompt="Update title:" v-param="title">✎</button>
                    <button v-delete="/api/items/{{item.id}}" v-confirm="Delete this item?">✕</button>
                </div>
            </div>
            {{#else}}
            <p class="empty">No items found. Add one above!</p>
            {{/#for}}
        </div>
    </Card>
</BaseLayout>
<style>
    .crud-form { display: flex; gap: 0.75rem; margin-bottom: 1.5rem; }
    .crud-form input { flex: 1; padding: 0.625rem; background: #0f172a; border: 1px solid #334155; border-radius: 8px; color: #f8fafc; }
    .crud-form button { padding: 0.625rem 1.25rem; background: #38bdf8; color: #0f172a; border: none; border-radius: 8px; font-weight: 600; cursor: pointer; }
    .item-row { display: flex; justify-content: space-between; align-items: center; padding: 0.875rem; border-bottom: 1px solid #334155; }
    .actions { display: flex; gap: 0.5rem; }
    .actions button { background: transparent; border: 1px solid #475569; color: #94a3b8; padding: 0.25rem 0.5rem; border-radius: 6px; cursor: pointer; }
    .actions button:hover { border-color: #38bdf8; color: #38bdf8; }
    .empty { color: #64748b; text-align: center; padding: 1rem; }
</style>"#,
        )
        .expect("Failed to write home.vlo");
    }

    println!("✅ Project initialized successfully!");
}