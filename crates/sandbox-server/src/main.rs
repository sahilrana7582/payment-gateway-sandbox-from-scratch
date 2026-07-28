//! Entry point. The ONLY crate that uses `anyhow` — every library crate below
//! returns typed errors; the binary is where they finally become exit codes.
//!
//! Usage:
//!   sandbox-server          run the gateway
//!   sandbox-server seed     create the demo merchant, print keys once

mod config;
mod seed;
mod startup;
mod telemetry;

use anyhow::Result;
use envconfig::Envconfig;

use crate::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    // .env is a dev convenience; real deployments set real env vars.
    dotenvy::dotenv().ok();
    telemetry::init();

    let config = Config::init_from_env()?;

    match std::env::args().nth(1).as_deref() {
        Some("seed") => seed::run(&config).await,
        Some(other) => {
            eprintln!("unknown command '{other}'. Commands: seed (or none, to serve)");
            std::process::exit(2);
        }
        None => startup::run(config).await,
    }
}
