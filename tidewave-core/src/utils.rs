//! Shared utility functions.

use rustls::ServerConfig;
use rustls_pemfile::{certs, pkcs8_private_keys};
use std::fs;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

pub fn load_tls_config_from_paths(
    cert_path_str: &str,
    key_path_str: &str,
) -> Result<Arc<ServerConfig>, Box<dyn std::error::Error + Send + Sync>> {
    let cert_path = PathBuf::from(cert_path_str);
    let key_path = PathBuf::from(key_path_str);

    info!(
        "Loading TLS certificates from {:?} and {:?}",
        cert_path, key_path
    );
    load_tls_config(&cert_path, &key_path)
}

fn load_tls_config(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<Arc<ServerConfig>, Box<dyn std::error::Error + Send + Sync>> {
    // Load certificate
    let cert_file = fs::File::open(cert_path)?;
    let mut cert_reader = BufReader::new(cert_file);
    let cert_chain: Vec<_> = certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

    // Load private key
    let key_file = fs::File::open(key_path)?;
    let mut key_reader = BufReader::new(key_file);
    let mut keys = pkcs8_private_keys(&mut key_reader).collect::<Result<Vec<_>, _>>()?;

    if keys.is_empty() {
        return Err("No private key found".into());
    }

    let key = keys.remove(0);

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key.into())?;

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

/// Converts a WSL path to a Windows path using wslpath.
pub async fn wslpath_to_windows(wsl_path: &str) -> Result<String, String> {
    use std::process::Stdio;
    use tokio::process::Command;

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut cmd = Command::new("wsl.exe");
        cmd.arg("-e").arg("wslpath").arg("-w").arg(wsl_path);
        cmd
    };

    #[cfg(not(target_os = "windows"))]
    let mut command = {
        let mut cmd = Command::new("wslpath");
        cmd.arg("-w").arg(wsl_path);
        cmd
    };

    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = command
        .output()
        .await
        .map_err(|e| format!("Failed to run wslpath: {}", e))?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(path)
    } else {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("wslpath failed: {}", error))
    }
}

/// Normalizes a path, converting WSL paths to Windows paths if needed.
#[allow(unused_variables)]
pub async fn normalize_path(path: &str, is_wsl: bool) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    if is_wsl {
        return wslpath_to_windows(path).await;
    }

    Ok(path.to_string())
}
