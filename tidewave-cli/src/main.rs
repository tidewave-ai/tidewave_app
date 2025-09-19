use clap::{Parser, Subcommand};
use tracing::{debug, error, info};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long, default_value = "9999")]
    port: u16,

    #[arg(short, long)]
    debug: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Serve {
        #[arg(short, long, default_value = "9999")]
        port: u16,

        #[arg(short, long)]
        debug: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let (debug, port) = match &cli.command {
        Some(Commands::Serve { debug, port }) => (*debug, *port),
        None => (cli.debug, cli.port),
    };

    let filter = if debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    if debug {
        debug!("Debug logging enabled");
        debug!("Port: {}", port);
    }

    info!("Starting server on port {}", port);

    if let Err(e) = tidewave_core::start_http_server(port).await {
        error!("Server error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}