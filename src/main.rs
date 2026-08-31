// ============================================================
// 1. VLO IMPORTS & MODULE DECLARATIONS
// ============================================================

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

// ============================================================
// 2. VLO CLI & APPLICATION ENTRY
// ============================================================

#[derive(Parser)]
#[command(
    name = "vlo",
    author = "VLO Team",
    version,
    about = "⚡ VLO v0.7 - Ultra-fast, component-driven Web Framework",
    long_about = "VLO combines component rendering, instant dynamic SQL APIs, hot module reloading (HMR), and simple static deployment into a single high-performance CLI framework."
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
            database::init_db().await;
            server::dev(host, port).await;
        }
        server::Commands::Build => {
            database::init_db().await;
            server::build();
        }
        server::Commands::Deploy { ref provider } => {
            database::init_db().await;
            server::deploy(provider).await;
        }
    }
}