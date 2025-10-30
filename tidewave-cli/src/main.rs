use clap::Parser;
use std::collections::HashMap;
use tracing::{debug, error};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    port: Option<u16>,

    #[arg(short, long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let config = tidewave_core::Config {
        port: cli.port.unwrap_or(9832),
        debug: cli.debug,
        env: HashMap::new(),
    };

    let filter = if config.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    if config.debug {
        debug!("Debug logging enabled");
        debug!("Config: {:?}", config);
    }

    // Set environment variables from config before server initialization
    for (key, value) in &config.env {
        debug!("Setting env var: {}={}", key, value);
        std::env::set_var(key, value);
    }

    if let Err(e) = tidewave_core::start_http_server(config).await {
        error!("Server error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
