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

/// Kills the process and all its children using Windows Job Objects.
/// Creates a job object, assigns the process to it, then terminates the job.
#[cfg(windows)]
pub async fn kill_process_group(child: &mut Child) -> std::io::Result<()> {
    use std::mem;
    use std::ptr;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
    };
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, PROCESS_ALL_ACCESS,
    };

    let Some(pid) = child.id() else {
        debug!("Process already exited (no PID)");
        return Ok(());
    };

    debug!("Killing process tree with PID: {} using Job Object", pid);

    unsafe {
        // Create a job object
        let job = CreateJobObjectW(ptr::null_mut(), ptr::null());
        if job.is_null() {
            let err = GetLastError();
            debug!("Failed to create job object (error {}), falling back to simple kill", err);
            return child.kill().await;
        }
        debug!("Created job object successfully");

        // Configure the job to kill all processes when the job is closed
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let set_result = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *mut _,
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );

        if set_result == 0 {
            let err = GetLastError();
            debug!("Failed to configure job object (error {}), falling back to simple kill", err);
            CloseHandle(job);
            return child.kill().await;
        }
        debug!("Configured job object with KILL_ON_JOB_CLOSE");

        // Open a handle to the process
        let process_handle = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
        if process_handle.is_null() {
            let err = GetLastError();
            debug!("Failed to open process handle (error {}), falling back to simple kill", err);
            CloseHandle(job);
            return child.kill().await;
        }
        debug!("Opened process handle for PID {}", pid);

        // Assign the process to the job
        let assign_result = AssignProcessToJobObject(job, process_handle);
        CloseHandle(process_handle);

        if assign_result == 0 {
            let err = GetLastError();
            // This can fail if the process is already in a job that doesn't allow nesting
            debug!(
                "Failed to assign process to job object (error {}), falling back to simple kill",
                err
            );
            CloseHandle(job);
            return child.kill().await;
        }
        debug!("Assigned process {} to job object", pid);

        // Terminate all processes in the job
        let terminate_result = TerminateJobObject(job, 1);
        if terminate_result == 0 {
            let err = GetLastError();
            debug!("TerminateJobObject failed (error {})", err);
        } else {
            debug!("TerminateJobObject succeeded");
        }

        // Close the job handle (this also triggers KILL_ON_JOB_CLOSE)
        CloseHandle(job);
        debug!("Closed job handle");
    }

    // Wait for the child to be reaped
    debug!("Waiting for child process to exit");
    let _ = child.wait().await;
    debug!("Child process exited");
    Ok(())
}
