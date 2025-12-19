use std::collections::HashMap;
use std::path::Path;
use tokio::process::{Child, Command};
use tracing::debug;

/// Wrapper that holds a child process and its associated job object (Windows only).
/// On Windows, the job object ensures all child processes are killed when terminated.
pub struct ChildProcess {
    pub child: Child,
    #[cfg(windows)]
    job_handle: Option<JobHandle>,
}

#[cfg(windows)]
struct JobHandle(winapi::um::winnt::HANDLE);

#[cfg(windows)]
unsafe impl Send for JobHandle {}
#[cfg(windows)]
unsafe impl Sync for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0);
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        // Only kill if the process is still running
        if let Some(pid) = self.child.id() {
            debug!("ChildProcess dropped, killing process tree for PID {}", pid);

            #[cfg(windows)]
            {
                // Explicitly terminate the job object to kill all child processes.
                // Note: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE would also kill processes when
                // JobHandle::Drop calls CloseHandle, but we terminate explicitly for clarity.
                if let Some(ref job) = self.job_handle {
                    unsafe {
                        winapi::um::jobapi2::TerminateJobObject(job.0, 1);
                    }
                }
                // Also try to kill the main process
                let _ = self.child.start_kill();
            }

            #[cfg(unix)]
            {
                // On Unix, kill the process group synchronously
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
}

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

/// Spawns a command and wraps it in a ChildProcess.
/// On Windows, this creates a job object to track all child processes.
/// On Unix, the process group is already set up by create_shell_command.
#[cfg(windows)]
pub fn spawn_command(mut command: Command) -> std::io::Result<ChildProcess> {
    use std::mem;
    use std::ptr;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, PROCESS_ALL_ACCESS,
    };

    // First, create and configure the job object
    let job_handle = unsafe {
        let job = CreateJobObjectW(ptr::null_mut(), ptr::null());
        if job.is_null() {
            debug!("Failed to create job object (error {})", GetLastError());
            None
        } else {
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
                debug!("Failed to configure job object (error {})", GetLastError());
                winapi::um::handleapi::CloseHandle(job);
                None
            } else {
                debug!("Created and configured job object");
                Some(JobHandle(job))
            }
        }
    };

    // Spawn the child process
    let child = command.spawn()?;

    // If we have a job handle, assign the process to it immediately
    if let (Some(ref job), Some(pid)) = (&job_handle, child.id()) {
        unsafe {
            let process_handle = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
            if !process_handle.is_null() {
                let assign_result = AssignProcessToJobObject(job.0, process_handle);
                winapi::um::handleapi::CloseHandle(process_handle);
                if assign_result != 0 {
                    debug!("Assigned process {} to job object", pid);
                } else {
                    debug!("Failed to assign process to job (error {})", GetLastError());
                }
            }
        }
    }

    Ok(ChildProcess { child, job_handle })
}

#[cfg(not(windows))]
pub fn spawn_command(mut command: Command) -> std::io::Result<ChildProcess> {
    let child = command.spawn()?;
    Ok(ChildProcess { child })
}
