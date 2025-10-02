use clap::{Parser, Subcommand};
use tracing::{debug, error};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long)]
    port: Option<u16>,

    #[arg(short, long)]
    debug: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Serve {
        #[arg(short, long)]
        port: Option<u16>,

        #[arg(short, long)]
        debug: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let (cli_debug, cli_port) = match &cli.command {
        Some(Commands::Serve { debug, port }) => (*debug, *port),
        None => (cli.debug, cli.port),
    };

    let config = tidewave_core::Config {
        port: cli_port.unwrap_or(9832),
        debug: cli_debug,
    };

    let filter = if config.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    if config.debug {
        debug!("Debug logging enabled");
        debug!("Config: {:?}", config);
    }

    if let Err(e) = tidewave_core::start_http_server(config, Box::new(|| {})).await {
        error!("Server error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
