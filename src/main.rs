// ─────────────────────────────────────────────────────────────
// main.rs — VLO CLI entry point
// ─────────────────────────────────────────────────────────────
#[macro_use]
mod state;
mod database;
mod api;
mod component;
mod template;
mod router;
mod server;
mod utils;
mod files;
mod files_api;
mod auth;
mod modules;
mod module_handler;
mod modifier;
mod mailer;
mod features;
mod jobs;
mod schema;
mod fs_api;
mod check;

use clap::Parser;

use crate::features::{init_features, is_enabled, Feature};

#[derive(Parser)]
#[command(
    name = "vlo",
    author = "VLO Team",
    version,
    about = "⚡ VLO - Ultra-fast, component-driven Web Framework",
    long_about = "VLO combines component rendering, SSR, dynamic SQL APIs, hot module reloading, and production deployment into the single runtime."
)]
struct Cli {
    #[command(subcommand)]
    command: server::Commands,
}

#[tokio::main]
async fn main() {
    // Load .env BEFORE anything else so LazyLock statics (VLO_DEBUG, etc.)
    // see the final environment on first access.
    dotenvy::dotenv().ok();

    crate::state::init_start_time();

    if let Err(error) = run().await {
        eprintln!("❌ {}", error);
        std::process::exit(1);
    }
}

/// Enable feature-gated subsystems. `is_build` suppresses long-lived
/// background workers (Jobs, SSE) that would otherwise linger during
/// one-shot commands.
async fn init_enabled_features(is_build: bool) -> Result<(), String> {
    // Canonical root URL is needed by mailer templates + API handlers.
    state::init_root_url();

    if is_enabled(Feature::Database) {
        database::init_db()
            .await
            .map_err(|e| format!("Database init failed: {}", e))?;
    }
    if is_enabled(Feature::Auth) {
        auth::init_auth_config()?;
        auth::init_session_secret()?;
    }
    if is_enabled(Feature::Modules) {
        modules::init_modules().map_err(|e| format!("Modules init failed: {}", e))?;
    }

    // Long-lived background workers: only spawn when a server actually runs.
    if is_enabled(Feature::Jobs) && !is_build {
    jobs::ensure_schema().await
        .map_err(|e| format!("Jobs schema init failed: {}", e))?;
    }

    if !is_build {
        if is_enabled(Feature::Jobs) {
            tokio::spawn(crate::jobs::start_worker());
        }
        if is_enabled(Feature::Sse) {
            crate::router::init_live_broadcast();
        }
    }

    Ok(())
}

async fn run() -> Result<(), String> {
    init_features()?;

    let cli = Cli::parse();

    match cli.command {
        server::Commands::Init { ref name, ref db, ref db_name, no_db } => {
            utils::init_project(name, db, db_name.as_deref(), no_db)?;
        }

        server::Commands::Dev { port, ref host } => {
            state::set_app_mode(state::AppMode::Development);
            init_enabled_features(false).await?;
            server::dev(host.as_deref(), port).await?;
        }

        server::Commands::Build { release } => {
            state::set_app_mode(state::AppMode::Production);
            init_enabled_features(true).await?;
            server::build(release)?;
        }

        server::Commands::Serve { port, ref host } => {
            state::set_app_mode(state::AppMode::Production);
            init_enabled_features(false).await?;
            server::serve(host.as_deref(), port).await?;
        }

        server::Commands::Schema { ref driver, ref out } => {
            let sql = database::dump_schema(driver)?;
            match out {
                Some(file) => {
                    std::fs::write(file, &sql)
                        .map_err(|e| format!("Cannot write {}: {}", file, e))?;
                    println!("✅ Wrote {} ({} bytes)", file, sql.len());
                }
                None => print!("{}", sql),
            }
        }

        server::Commands::Check { json, strict } => {
            check::run(json, strict)?;
        }

        server::Commands::Deploy { ref provider } => {
            state::set_app_mode(state::AppMode::Production);
            init_enabled_features(false).await?;
            server::deploy(provider).await?;
        }

        server::Commands::Cgi => {
            state::set_app_mode(state::AppMode::Production);
            init_enabled_features(false).await?;
            server::cgi().await?;
        }
    }

    Ok(())
}