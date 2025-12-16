use std::collections::HashMap;
use std::path::Path;
use tokio::process::{Child, Command};
use tracing::debug;

fn command_with_limited_env(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("APPIMAGE")
        .env_remove("APPDIR");
    command
}

pub fn create_shell_command(
    cmd: &str,
    env: HashMap<String, String>,
    cwd: &str,
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))] is_wsl: bool,
) -> Command {
    #[cfg(target_os = "windows")]
    {
        if is_wsl {
            // WSL case: use --cd flag and construct env string
            // Build env assignments string: VAR1=value1 VAR2=value2 ... command
            let env_string: Vec<String> = env
                .iter()
                .map(|(k, v)| {
                    // Escape single quotes in the value by replacing ' with '\''
                    let escaped_value = v.replace("'", "'\\''");
                    format!("{}='{}'", k, escaped_value)
                })
                .collect();

            let full_command = if env_string.is_empty() {
                cmd.to_string()
            } else {
                format!("{} {}", env_string.join(" "), cmd)
            };

            let mut command = command_with_limited_env("wsl.exe");
            command
                .arg("--cd")
                .arg(cwd)
                .arg("sh")
                .arg("-c")
                .arg(full_command)
                .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
            command
        } else {
            // Windows cmd case: use .current_dir()
            let mut command = command_with_limited_env("cmd.exe");
            command
                .arg("/s")
                .arg("/c")
                .arg(cmd)
                .envs(env)
                .current_dir(Path::new(cwd))
                .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
            command
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Unix case: use .current_dir()
        // Also create a new process group so we can kill all descendants
        let mut command = command_with_limited_env("sh");
        command
            .arg("-c")
            .arg(cmd)
            .envs(env)
            .current_dir(Path::new(cwd))
            .process_group(0);
        command
    }
}

/// Kills the process and all its children by killing the process group.
/// On Unix, we use kill(-pgid, SIGKILL) to kill the entire process group.
/// On Windows, we fall back to just killing the child process.
#[cfg(unix)]
pub async fn kill_process_group(child: &mut Child) -> std::io::Result<()> {
    if let Some(pid) = child.id() {
        debug!("Killing process group with PGID: {}", pid);
        // Kill the entire process group (negative PID means process group)
        let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        if result == 0 {
            // Wait for the child to be reaped
            let _ = child.wait().await;
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    } else {
        // Process already exited
        Ok(())
    }
}

#[cfg(not(unix))]
pub async fn kill_process_group(child: &mut Child) -> std::io::Result<()> {
    child.kill().await
}
