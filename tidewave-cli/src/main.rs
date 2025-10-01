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

    let mut config = match tidewave_core::load_config() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    let (cli_debug, cli_port) = match &cli.command {
        Some(Commands::Serve { debug, port }) => (*debug, *port),
        None => (cli.debug, cli.port),
    };

    // CLI args override config file
    if cli_debug {
        config.debug = true;
    }
    if let Some(port) = cli_port {
        config.port = port;
    }

    let filter = if config.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    if config.debug {
        debug!("Debug logging enabled");
        debug!("Config: {:?}", config);
    }

    if let Err(e) = tidewave_core::start_http_server(config).await {
        error!("Server error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
