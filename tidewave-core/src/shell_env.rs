//! Capturing the user's shell environment for a given directory.
//!
//! Spawns the user's default shell as an interactive login shell attached to
//! a PTY, with the target directory as the working directory, and makes it
//! dump its environment to a temporary file. Because the shell goes through
//! its full interactive startup (profile and rc files, prompt hooks), the
//! captured environment matches what a terminal opened in that directory
//! would have, including variables injected by tools such as direnv or mise,
//! whose hooks only run when a prompt is actually rendered.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

/// How long to wait for the shell to start up and dump its environment.
/// Interactive startup can be slow (heavy rc files, WSL cold start).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// Returns the environment that an interactive login shell has in `cwd`.
///
/// On native Windows there are no shell rc files to source and the process
/// environment already includes the user's registry environment, so it is
/// returned as is. With `wsl_distro` set, the capture runs inside the distro
/// and `cwd` is expected to be a path within it.
pub async fn capture_shell_env(
    cwd: &str,
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))] wsl_distro: Option<&str>,
) -> Result<HashMap<String, String>, String> {
    #[cfg(target_os = "windows")]
    {
        if let Some(distro) = wsl_distro {
            capture_wsl_env(cwd, distro).await
        } else {
            // vars() panics on non-Unicode values, which are valid on Windows
            Ok(std::env::vars_os()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect())
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        capture_unix_env(cwd).await
    }
}

#[cfg(not(target_os = "windows"))]
async fn capture_unix_env(cwd: &str) -> Result<HashMap<String, String>, String> {
    if !std::path::Path::new(cwd).is_dir() {
        return Err(format!("Directory does not exist: {}", cwd));
    }

    let dump_path = std::env::temp_dir().join(format!("tidewave-env-{}", uuid::Uuid::new_v4()));
    let dump_path_str = dump_path.to_string_lossy().into_owned();

    let mut cmd = CommandBuilder::new_default_prog();
    cmd.cwd(cwd);

    let pty_result = run_in_pty(cmd, &dump_script(&dump_path_str)).await;

    let dump = tokio::fs::read(&dump_path).await;
    let _ = tokio::fs::remove_file(&dump_path).await;

    pty_result?;
    let bytes = dump.map_err(|e| format!("Shell did not dump its environment: {}", e))?;
    if bytes.is_empty() {
        return Err("Shell dumped an empty environment".to_string());
    }
    Ok(parse_env_dump(&bytes))
}

#[cfg(target_os = "windows")]
async fn capture_wsl_env(cwd: &str, distro: &str) -> Result<HashMap<String, String>, String> {
    use std::process::Stdio;

    let dump_path = format!("/tmp/tidewave-env-{}", uuid::Uuid::new_v4());

    // WSL sets SHELL to the user's login shell when launching commands
    let mut cmd = CommandBuilder::new("wsl.exe");
    cmd.arg("-d");
    cmd.arg(distro);
    cmd.arg("--cd");
    cmd.arg(cwd);
    cmd.arg("sh");
    cmd.arg("-c");
    cmd.arg("exec \"${SHELL:-sh}\" -l");

    let pty_result = run_in_pty(cmd, &dump_script(&dump_path)).await;

    // Read the dump back out of the distro's filesystem. wsl.exe passes the
    // Linux process output through verbatim, so there are no UTF-16 concerns.
    let mut command = crate::command::command_with_limited_env("wsl.exe");
    command
        .arg("-d")
        .arg(distro)
        .arg("sh")
        .arg("-c")
        .arg(format!("cat '{path}'; rm -f '{path}'", path = dump_path))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
    let dump = command
        .output()
        .await
        .map_err(|e| format!("Failed to read environment dump: {}", e));

    pty_result?;
    let output = dump?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(format!(
            "Shell did not dump its environment: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_env_dump(&output.stdout))
}

/// The line fed to the shell's stdin to dump its environment.
///
/// Notes:
///   * /usr/bin/env (rather than plain env) bypasses shell aliases and
///     functions in every shell.
///   * -0 (NUL separators) keeps values containing newlines parseable. We rely
///     on env supporting -0, which coreutils, macOS and the BSDs all do.
///   * umask 077 keeps the dump file, which may contain secrets, readable
///     only by the user, regardless of rc files changing the umask.
///   * Restricted to syntax shared by POSIX shells, fish and csh: > and ;
///     (umask is a builtin in all of them).
///   * The leading space keeps the command out of history in shells
///     configured to ignore space-prefixed commands.
fn dump_script(path: &str) -> String {
    format!(" umask 077; /usr/bin/env -0 >'{path}'; exit\n")
}

/// Spawns the command attached to a PTY, feeds `input` to its stdin and waits
/// for the shell process to exit, discarding all terminal output. Force-kills
/// the shell if it does not exit within [`CAPTURE_TIMEOUT`].
async fn run_in_pty(cmd: CommandBuilder, input: &str) -> Result<(), String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("Failed to open PTY: {}", e))?;

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn shell: {}", e))?;
    drop(pair.slave);

    let child_pid = child.process_id();
    let mut killer = child.clone_killer();

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {}", e))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to take PTY writer: {}", e))?;

    // The PTY buffers the input until the shell reads it, after its
    // interactive startup completes
    writer
        .write_all(input.as_bytes())
        .map_err(|e| format!("Failed to write to PTY: {}", e))?;

    // Drain the shell's output so a large interactive banner cannot fill the
    // PTY buffer and block the shell before it dumps and exits, and answer the
    // queries ConPTY sends on startup (see `conpty`). This is
    // best-effort cleanup, not the completion signal: it is detached and never
    // joined, and it ends once the process group is killed below closes the
    // slave. PTY reads are blocking, so it runs on a blocking thread.
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        #[cfg(windows)]
        let mut queries = conpty::QueryScanner::default();
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            #[cfg(windows)]
            {
                let replies = queries.replies(&buf[..n]);
                if !replies.is_empty() {
                    // Best effort: if this fails the shell is most likely gone
                    // already, and draining is still worthwhile
                    let _ = writer.write_all(&replies).and_then(|()| writer.flush());
                }
            }
        }
        // The writer lives here so the queries can be answered. It goes away
        // together with the reader, once the PTY is closed.
        drop(writer);
    });

    // Wait for the shell process itself to exit, rather than for PTY EOF: a
    // background process an rc file spawns can inherit the slave and hold it
    // open long after the shell is gone, which would otherwise stall us until
    // the timeout. The dump is written before the shell's `exit`, so once it
    // exits the file is ready.
    let wait_handle = tokio::task::spawn_blocking(move || child.wait());
    let wait_result = tokio::time::timeout(CAPTURE_TIMEOUT, wait_handle).await;

    // Force-kill the shell's process group either way: on timeout to stop a
    // wedged shell, on success to reap any lingering background job it spawned
    // (which also lets the detached drain thread finish).
    kill_process_group(child_pid, killer.as_mut());

    match wait_result {
        Ok(join_result) => {
            join_result
                .map_err(|e| format!("Shell wait task failed: {}", e))?
                .map_err(|e| format!("Failed to wait for shell: {}", e))?;
            Ok(())
        }
        Err(_) => Err(format!(
            "Shell did not exit within {} seconds",
            CAPTURE_TIMEOUT.as_secs()
        )),
    }
}

/// The handshake ConPTY performs with the terminal on startup.
///
/// portable-pty creates its pseudoconsoles with `PSEUDOCONSOLE_INHERIT_CURSOR`,
/// which makes conhost query the terminal as soon as the PTY opens and hold
/// the child's console until it gets an answer: older builds send a cursor
/// position query and wait for its report indefinitely, newer ones also send
/// a primary device attributes query and wait up to a second for its response.
/// Everywhere else in the app a terminal emulator is on the other end of the
/// PTY and answers (xterm.js, for the app's own terminal), but here the only
/// reader is the drain task, so it has to answer itself or the shell never
/// starts up and every capture ends in `CAPTURE_TIMEOUT`. The answers are made
/// up, but nothing here draws to the terminal, so nothing depends on them.
///
/// Unix PTYs never originate output, so on other platforms none of this is
/// used.
mod conpty {
    #![cfg_attr(not(windows), allow(dead_code))]

    /// The queries conhost sends, each with the reply we answer it with:
    ///
    ///   * Device Status Report asking for the cursor position, answered with
    ///     a Cursor Position Report of row 1, column 1.
    ///   * Primary Device Attributes, answered with the same plain VT100
    ///     response xterm.js gives. conhost only records the reply; a
    ///     conformance level this low advertises no extra features.
    pub const QUERIES: [(&[u8], &[u8]); 2] =
        [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?1;2c")];

    /// Finds [`QUERIES`] across successive chunks of PTY output, keeping the
    /// end of the previous chunk so a query split across two reads is still
    /// found.
    #[derive(Default)]
    pub struct QueryScanner {
        tail: Vec<u8>,
    }

    impl QueryScanner {
        /// Returns the replies to the queries `chunk` completes, in order.
        pub fn replies(&mut self, chunk: &[u8]) -> Vec<u8> {
            let previous_len = self.tail.len();
            self.tail.extend_from_slice(chunk);

            let mut replies = Vec::new();
            for start in 0..self.tail.len() {
                for (query, reply) in QUERIES {
                    // Only a query ending in this chunk is new; one ending in
                    // the kept tail was already answered with the previous one
                    if self.tail[start..].starts_with(query) && start + query.len() > previous_len {
                        replies.extend_from_slice(reply);
                    }
                }
            }

            // Keep only what could still be the start of a query
            let keep = QUERIES
                .iter()
                .map(|(query, _)| query.len())
                .max()
                .unwrap_or(1)
                - 1;
            let excess = self.tail.len().saturating_sub(keep);
            self.tail.drain(..excess);

            replies
        }
    }
}

/// Force-terminates the shell and everything it spawned.
///
/// On Unix the shell is a session/process-group leader (portable-pty calls
/// `setsid`), so signalling the negative pid reaches the whole group, including
/// background jobs an rc file started; plain [`ChildKiller::kill`] would send a
/// single SIGHUP to only the shell, which a job that traps HUP survives.
fn kill_process_group(pid: Option<u32>, killer: &mut dyn portable_pty::ChildKiller) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // Safe: kill(2) with a negative pid and SIGKILL has no memory effects.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        return;
    }

    let _ = pid;
    let _ = killer.kill();
}

/// Parses NUL-separated `env -0` output. A NUL after every entry (including the
/// last) means values may contain newlines or `=` without ambiguity.
fn parse_env_dump(bytes: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut env = HashMap::new();

    for entry in text.split('\0') {
        if let Some((key, value)) = entry.split_once('=') {
            if !key.is_empty() {
                env.insert(key.to_string(), value.to_string());
            }
        }
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_dump_nul_separated() {
        let env = parse_env_dump(b"FOO=bar\0MULTI=line1\nline2\0EMPTY=\0EQ=a=b\0");
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(env.get("MULTI"), Some(&"line1\nline2".to_string()));
        assert_eq!(env.get("EMPTY"), Some(&"".to_string()));
        assert_eq!(env.get("EQ"), Some(&"a=b".to_string()));
        assert_eq!(env.len(), 4);
    }

    #[test]
    fn query_scanner_answers_queries_across_chunks() {
        let mut scanner = conpty::QueryScanner::default();
        assert_eq!(scanner.replies(b"no query here"), b"");
        // The startup handshake: cursor position, then device attributes
        assert_eq!(
            scanner.replies(b"\x1b[6n\x1b[c\x1b[?1004h\x1b[?9001h"),
            b"\x1b[1;1R\x1b[?1;2c"
        );
        // The queries just matched must not be answered again with the next
        // chunk, even the short one that fits in the kept tail whole
        assert_eq!(scanner.replies(b"more output"), b"");
        assert_eq!(scanner.replies(b"banner\x1b[c"), b"\x1b[?1;2c");
        assert_eq!(scanner.replies(b"prompt"), b"");

        // A query split across two reads is still answered, once
        let mut scanner = conpty::QueryScanner::default();
        assert_eq!(scanner.replies(b"output\x1b["), b"");
        assert_eq!(scanner.replies(b"6n"), b"\x1b[1;1R");
        assert_eq!(scanner.replies(b"n"), b"");
    }
}
