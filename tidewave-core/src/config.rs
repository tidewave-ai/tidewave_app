use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use tracing::debug;

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default)]
    pub debug: bool,
}

fn default_port() -> u16 {
    9999
}

pub fn get_config_path() -> PathBuf {
    let home = if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE").expect("USERPROFILE environment variable not set")
    } else {
        std::env::var("HOME").expect("HOME environment variable not set")
    };

    PathBuf::from(home).join(".config").join("tidewave").join("config.toml")
}

pub fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let config_path = get_config_path();

    if !config_path.exists() {
        debug!("Config file does not exist, using defaults");
        return Ok(Config {
            port: default_port(),
            debug: false,
        });
    }

    debug!("Loading config from: {:?}", config_path);

    let content = fs::read_to_string(&config_path)?;

    let error_message = r#"Expected configuration to be:

    port = 0..65535"#;

    let table = content.parse::<toml::Table>()
        .map_err(|_| error_message)?;

    let config = match (table.len(), table.get("port")) {
        (0, None) => Config {
            port: default_port(),
            debug: false,
        },
        (1, Some(toml::Value::Integer(port))) => {
            let port = u16::try_from(*port)
                .map_err(|_| error_message)?;
            Config {
                port,
                debug: false,
            }
        }
        _ => {
            return Err(error_message.into());
        }
    };

    debug!("Loaded config: {:?}", config);
    Ok(config)
}
