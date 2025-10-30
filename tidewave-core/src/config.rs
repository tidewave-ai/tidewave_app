use serde::Deserialize;
use std::collections::HashMap;
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

    #[serde(default)]
    pub allow_remote_access: bool,

    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_port() -> u16 {
    9832
}

pub fn get_config_path() -> PathBuf {
    let home = if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE").expect("USERPROFILE environment variable not set")
    } else {
        std::env::var("HOME").expect("HOME environment variable not set")
    };

    PathBuf::from(home)
        .join(".config")
        .join("tidewave")
        .join("app.toml")
}

pub fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let config_path = get_config_path();

    if !config_path.exists() {
        debug!("Config file does not exist, using defaults");
        return Ok(Config {
            port: default_port(),
            debug: false,
            allow_remote_access: false,
            env: HashMap::new(),
        });
    }

    debug!("Loading config from: {:?}", config_path);

    let content = fs::read_to_string(&config_path)?;

    // Use serde to deserialize the full config
    let config: Config = toml::from_str(&content).map_err(|e| {
        format!(
            "Failed to parse config: {}\n\nExpected format:\nport = 9832\ndebug = false\nallow_remote_access = false\n\n[env]\nKEY = \"value\"",
            e
        )
    })?;

    debug!("Loaded config: {:?}", config);
    Ok(config)
}
