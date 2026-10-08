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

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "vlo",
    author = "VLO Team",
    version,
    about = "⚡ VLO - Ultra-fast, component-driven Web Framework",
    long_about = "VLO combines component rendering, SSR, dynamic SQL APIs, hot module reloading, and production deployment into a single runtime."
)]
struct Cli {
    #[command(subcommand)]
    command: server::Commands,
}

#[tokio::main]
async fn main() {
    crate::state::init_start_time();
    if let Err(error) = run().await {
        eprintln!("❌ {}", error);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();

    match cli.command {
        server::Commands::Init {
            ref name,
            ref db,
            ref db_name,
            no_db,
        } => {
            utils::init_project(name, db, db_name.as_deref(), no_db)?;
        }

        server::Commands::Dev { port, ref host } => {
            state::set_app_mode(state::AppMode::Development);
            database::init_db().await?;
            auth::init_auth_config();
            auth::init_session_secret();
            modules::init_modules()?; 

            let port = port
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("Invalid port '{}'.", value))
                })
                .transpose()?;

            server::dev(host.as_deref(), port).await?;
        }

        server::Commands::Build { release } => {
            state::set_app_mode(state::AppMode::Production);
            database::init_db().await?;
            modules::init_modules()?; // 🔥 CRITICAL: Required to resolve module components (e.g., <PIECHART />) during build
            server::build(release)?;
        }

        server::Commands::Serve { port, ref host } => {
            state::set_app_mode(state::AppMode::Production);
            database::init_db().await?;
            auth::init_auth_config();
            auth::init_session_secret();
            crate::router::init_live_broadcast();
            modules::init_modules()?; 

            let port = port
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("Invalid port '{}'.", value))
                })
                .transpose()?;

            server::serve(host.as_deref(), port).await?;
        }

        server::Commands::Deploy { ref provider } => {
            state::set_app_mode(state::AppMode::Production);
            database::init_db().await?;
            modules::init_modules()?; // 🔥 CRITICAL: Ensures modules are loaded if deploy triggers an internal build
            crate::router::init_live_broadcast();
            server::deploy(provider).await?;
        }

        server::Commands::Cgi => {
            state::set_app_mode(state::AppMode::Production);
            database::init_db().await?;
            auth::init_auth_config();
            auth::init_session_secret();
            modules::init_modules()?; // 🔥 CRITICAL: Ensures components render correctly in CGI environments

            server::cgi().await?;
        }
    }

    Ok(())
}