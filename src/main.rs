#[macro_use]
mod state;
mod database;
mod api;
mod component;
mod template;
mod router;
mod server;
mod utils;

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
    let cli = Cli::parse();

    match cli.command {
        server::Commands::Init {
            ref name,
            ref db,
            ref db_name,
            no_db,
        } => {
            utils::init_project(name, db, db_name.as_deref(), no_db);
        }

        server::Commands::Dev { port, ref host } => {
            state::set_app_mode(state::AppMode::Development);
            database::init_db().await;
            server::dev(host, port).await;
        }

        server::Commands::Build { release } => {
            state::set_app_mode(state::AppMode::Production);
            database::init_db().await;
            server::build(release);
        }

        server::Commands::Serve { port, ref host } => {
            state::set_app_mode(state::AppMode::Production);
            database::init_db().await;
            server::serve(host, port).await;
        }

        server::Commands::Deploy { ref provider } => {
            state::set_app_mode(state::AppMode::Production);
            database::init_db().await;
            server::deploy(provider).await;
        }
    }
}