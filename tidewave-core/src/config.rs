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
    pub https_port: Option<u16>,

    #[serde(default)]
    pub https_cert_path: Option<String>,

    #[serde(default)]
    pub https_key_path: Option<String>,

    #[serde(default)]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

fn default_port() -> u16 {
    9832
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            debug: false,
            allow_remote_access: false,
            https_port: None,
            https_cert_path: None,
            https_key_path: None,
            env: HashMap::new(),
            allowed_origins: Vec::new(),
        }
    }
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
        return Ok(Config::default());
    }

    debug!("Loading config from: {:?}", config_path);

    let content = fs::read_to_string(&config_path)?;

    // Use serde to deserialize the full config
    let config: Config = toml::from_str(&content).map_err(|e| {
        format!(
            "Failed to parse config: {}\n\nExpected format:\nport = 9832\n# https_port = 9833\n# https_cert_path = \"/path/to/cert.pem\"\n# https_key_path = \"/path/to/key.pem\"\ndebug = false\nallow_remote_access = false\n\n[env]\nKEY = \"value\"",
            e
        )
    })?;

    debug!("Loaded config: {:?}", config);

    // Validate HTTPS configuration: all three options must be provided or none
    validate_https_config(&config)?;

    Ok(config)
}

fn validate_https_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let has_port = config.https_port.is_some();
    let has_cert = config.https_cert_path.is_some();
    let has_key = config.https_key_path.is_some();

    // All three must be present or all three must be absent
    if has_port || has_cert || has_key {
        if !has_port {
            return Err(
                "HTTPS configuration error: https_port is required when using custom certificates"
                    .into(),
            );
        }
        if !has_cert {
            return Err(
                "HTTPS configuration error: https_cert_path is required when https_port is set"
                    .into(),
            );
        }
        if !has_key {
            return Err(
                "HTTPS configuration error: https_key_path is required when https_port is set"
                    .into(),
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_config() {
        let toml_content = "";
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.port, 9832);
        assert_eq!(config.debug, false);
        assert_eq!(config.allow_remote_access, false);
        assert_eq!(config.https_port, None);
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_parse_config_with_env_section() {
        let toml_content = r#"
port = 8080
https_port = 8443
https_cert_path = "/path/to/cert.pem"
https_key_path = "/path/to/key.pem"
allow_remote_access = true
debug = true

[env]
API_KEY = "secret"
DATABASE_URL = "postgres://localhost"
# COMMENTED_VAR = "unknown"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.debug, true);
        assert_eq!(config.allow_remote_access, true);
        assert_eq!(config.https_port, Some(8443));
        assert_eq!(
            config.https_cert_path,
            Some("/path/to/cert.pem".to_string())
        );
        assert_eq!(config.https_key_path, Some("/path/to/key.pem".to_string()));
        assert_eq!(config.env.len(), 2);
        assert_eq!(config.env.get("API_KEY"), Some(&"secret".to_string()));
        assert_eq!(
            config.env.get("DATABASE_URL"),
            Some(&"postgres://localhost".to_string())
        );
    }

    #[test]
    fn test_validate_https_config_all_present() {
        let config = Config {
            https_port: Some(9833),
            https_cert_path: Some("/path/to/cert.pem".to_string()),
            https_key_path: Some("/path/to/key.pem".to_string()),
            ..Default::default()
        };
        assert!(validate_https_config(&config).is_ok());
    }

    #[test]
    fn test_validate_https_config_all_absent() {
        let config = Config::default();
        assert!(validate_https_config(&config).is_ok());
    }

    #[test]
    fn test_validate_https_missing_config() {
        let config = Config {
            https_port: Some(9833),
            https_cert_path: Some("/path/to/cert.pem".to_string()),
            ..Default::default()
        };
        assert!(validate_https_config(&config).is_err());
    }

    #[test]
    fn test_parse_config_with_allowed_origins() {
        let toml_content = r#"
port = 8080
allowed_origins = ["https://example.com", "https://app.example.com"]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.allowed_origins.len(), 2);
        assert_eq!(config.allowed_origins[0], "https://example.com".to_string());
        assert_eq!(
            config.allowed_origins[1],
            "https://app.example.com".to_string()
        );
    }
}
