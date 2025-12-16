use std::collections::HashMap;
use std::path::Path;
use tokio::process::Command;

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
