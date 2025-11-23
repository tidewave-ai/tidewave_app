use clap::Parser;
use tracing::{debug, error};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    port: Option<u16>,

    #[arg(short, long)]
    debug: bool,

    #[arg(short, long)]
    allow_remote_access: bool,

    #[arg(long)]
    https_port: Option<u16>,

    #[arg(long)]
    https_cert_path: Option<String>,

    #[arg(long)]
    https_key_path: Option<String>,

    #[arg(long, value_delimiter = ',')]
    allowed_origins: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let config = tidewave_core::Config {
        port: cli.port.unwrap_or(9832),
        debug: cli.debug,
        allow_remote_access: cli.allow_remote_access,
        https_port: cli.https_port,
        https_cert_path: cli.https_cert_path,
        https_key_path: cli.https_key_path,
        allowed_origins: cli.allowed_origins,
        ..Default::default()
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
