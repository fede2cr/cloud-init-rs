//! Hardened subprocess execution.
//!
//! Rules enforced here, once, for the whole tree:
//! * argv arrays only — there is no shell, so no interpolation vulnerabilities;
//! * an explicit, minimal environment by default (no inherited `PATH`/`IFS`/`LD_*`);
//! * a wall-clock timeout, because early boot must not hang on a wedged helper;
//! * capped output capture, because a runaway child must not exhaust memory.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Default ceiling on captured stdout/stderr, per stream (4 MiB).
pub const DEFAULT_MAX_OUTPUT: usize = 4 * 1024 * 1024;
/// Default wall-clock limit for a helper process.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to execute `{cmd}`: {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: io::Error,
    },
    #[error("`{cmd}` timed out after {}s", timeout.as_secs())]
    Timeout { cmd: String, timeout: Duration },
    #[error("i/o error while running `{cmd}`: {source}")]
    Io {
        cmd: String,
        #[source]
        source: io::Error,
    },
    #[error(
        "`{cmd}` exited with status {status}: {}",
        String::from_utf8_lossy(stderr)
    )]
    NonZeroExit {
        cmd: String,
        status: i32,
        stderr: Vec<u8>,
    },
}

/// Result of running a child process to completion.
#[derive(Debug, Clone)]
pub struct Output {
    /// Exit code, or `None` if the child was killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Set when captured output hit the cap and was truncated.
    pub truncated: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// Stdout as text with trailing newlines removed, mirroring `subp().stdout.strip()`.
    pub fn stdout_trimmed(&self) -> String {
        self.stdout_lossy().trim().to_owned()
    }
}

/// Builder for a hardened child process.
#[derive(Debug, Clone)]
pub struct Subp {
    argv: Vec<OsString>,
    env: BTreeMap<OsString, OsString>,
    inherit_env: bool,
    cwd: Option<PathBuf>,
    stdin: Option<Vec<u8>>,
    timeout: Option<Duration>,
    max_output: usize,
}

impl Subp {
    /// Start building a command. `argv[0]` is the program.
    pub fn new<S: AsRef<OsStr>>(argv: impl IntoIterator<Item = S>) -> Self {
        Self {
            argv: argv
                .into_iter()
                .map(|s| s.as_ref().to_os_string())
                .collect(),
            env: default_env(),
            inherit_env: false,
            cwd: None,
            stdin: None,
            timeout: Some(DEFAULT_TIMEOUT),
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }

    /// Add or override one environment variable.
    #[must_use]
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .insert(key.as_ref().to_os_string(), value.as_ref().to_os_string());
        self
    }

    /// Inherit the caller's environment.
    ///
    /// Only for helpers that documented-ly need it; the safe default is a clean env.
    #[must_use]
    pub fn inherit_env(mut self) -> Self {
        self.inherit_env = true;
        self
    }

    #[must_use]
    pub fn cwd(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    #[must_use]
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output = bytes;
        self
    }

    /// Run to completion, capturing output. A non-zero exit is *not* an error.
    pub fn run(self) -> Result<Output, Error> {
        let cmd_str = self.display();
        let mut command = self.command(&cmd_str)?;
        command
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|source| Error::Spawn {
            cmd: cmd_str.clone(),
            source,
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let cap = self.max_output;
        let out_reader = stdout.map(|s| thread::spawn(move || drain(s, cap)));
        let err_reader = stderr.map(|s| thread::spawn(move || drain(s, cap)));

        if let (Some(data), Some(mut pipe)) = (self.stdin.as_ref(), child.stdin.take())
        {
            use std::io::Write;
            // A closed pipe means the child exited early; that is its business.
            let _ = pipe.write_all(data);
        }

        let code = wait_with_timeout(&mut child, self.timeout, &cmd_str)?;

        let (stdout, out_trunc) = join(out_reader, &cmd_str)?;
        let (stderr, err_trunc) = join(err_reader, &cmd_str)?;

        Ok(Output {
            code,
            stdout,
            stderr,
            truncated: out_trunc || err_trunc,
        })
    }

    /// Run to completion and fail on a non-zero exit status.
    pub fn check(self) -> Result<Output, Error> {
        let cmd = self.display();
        let out = self.run()?;
        match out.code {
            Some(0) => Ok(out),
            Some(status) => Err(Error::NonZeroExit {
                cmd,
                status,
                stderr: out.stderr,
            }),
            None => Err(Error::NonZeroExit {
                cmd,
                status: -1,
                stderr: out.stderr,
            }),
        }
    }

    /// Run to completion on the caller's own stdio, capturing nothing.
    ///
    /// For helpers whose output belongs to the caller's console or journal
    /// rather than to the program, which is what a bare `cmd` in a shell
    /// script does. `stdin` data set on the builder is ignored, and no output
    /// cap applies because nothing is buffered.
    pub fn passthrough(self) -> Result<std::process::ExitStatus, Error> {
        let cmd_str = self.display();
        let mut command = self.command(&cmd_str)?;
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|source| Error::Spawn {
            cmd: cmd_str.clone(),
            source,
        })?;
        wait_status_with_timeout(&mut child, self.timeout, &cmd_str)
    }

    /// The argv/env/cwd half of the command, shared by every run mode.
    fn command(&self, cmd_str: &str) -> Result<Command, Error> {
        let mut command =
            Command::new(self.argv.first().ok_or_else(|| Error::Spawn {
                cmd: cmd_str.to_owned(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "empty argv"),
            })?);
        if let Some(args) = self.argv.get(1..) {
            command.args(args);
        }
        if !self.inherit_env {
            command.env_clear();
        }
        command.envs(&self.env);
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        Ok(command)
    }

    fn display(&self) -> String {
        self.argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Convenience wrapper: run `argv` with all defaults.
pub fn run<S: AsRef<OsStr>>(
    argv: impl IntoIterator<Item = S>,
) -> Result<Output, Error> {
    Subp::new(argv).run()
}

/// `subp.which`: the first executable `program` on `PATH`.
///
/// A program that already contains a separator is not searched for, and only
/// absolute `PATH` entries are considered, both as upstream does.
#[must_use]
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return is_exe(Path::new(program)).then(|| PathBuf::from(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_exe(candidate))
}

/// `subp.which(program, search=…)`: the first executable `program` under one
/// of `search`.
///
/// The list *replaces* `PATH` rather than extending it, which is the whole
/// point of upstream's `search` argument: the network renderer probes look in
/// `/sbin` and `/usr/sbin` whether or not the caller's `PATH` has them, and
/// nowhere else.
#[must_use]
pub fn which_in(program: &str, search: &[&str]) -> Option<PathBuf> {
    if program.contains('/') {
        return is_exe(Path::new(program)).then(|| PathBuf::from(program));
    }
    search
        .iter()
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| is_exe(candidate))
}

/// `util.is_exe`: a regular file with an execute bit set.
///
/// The bit, not `access(2)`: upstream asks `os.access(p, os.X_OK)`, which
/// answers for the calling user, but every caller here runs as root, for whom
/// the two agree.
#[must_use]
pub fn is_exe(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn wait_with_timeout(
    child: &mut Child,
    timeout: Option<Duration>,
    cmd: &str,
) -> Result<Option<i32>, Error> {
    Ok(wait_status_with_timeout(child, timeout, cmd)?.code())
}

fn wait_status_with_timeout(
    child: &mut Child,
    timeout: Option<Duration>,
    cmd: &str,
) -> Result<std::process::ExitStatus, Error> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(source) => {
                return Err(Error::Io {
                    cmd: cmd.to_owned(),
                    source,
                })
            }
        }
        if let (Some(deadline), Some(timeout)) = (deadline, timeout) {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Timeout {
                    cmd: cmd.to_owned(),
                    timeout,
                });
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

type Drained = (Vec<u8>, bool);

/// Read a pipe to EOF, keeping at most `cap` bytes but always draining so the
/// child never blocks on a full pipe.
fn drain(mut src: impl Read, cap: usize) -> io::Result<Drained> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let n = src.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let Some(data) = chunk.get(..n) else { break };
        let room = cap.saturating_sub(kept.len());
        if room == 0 {
            truncated = true;
            continue;
        }
        if let Some(fits) = data.get(..room.min(n)) {
            kept.extend_from_slice(fits);
        }
        if n > room {
            truncated = true;
        }
    }
    Ok((kept, truncated))
}

fn join(
    handle: Option<thread::JoinHandle<io::Result<Drained>>>,
    cmd: &str,
) -> Result<Drained, Error> {
    let Some(handle) = handle else {
        return Ok((Vec::new(), false));
    };
    handle
        .join()
        .map_err(|_| Error::Io {
            cmd: cmd.to_owned(),
            source: io::Error::other("output reader thread panicked"),
        })?
        .map_err(|source| Error::Io {
            cmd: cmd.to_owned(),
            source,
        })
}

/// Minimal, deterministic environment.
///
/// `LANG`/`LC_ALL=C` matches cloud-init, which parses helper output and must not be
/// affected by the instance locale.
fn default_env() -> BTreeMap<OsString, OsString> {
    [
        ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
        ("LANG", "C"),
        ("LC_ALL", "C"),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
    .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout_and_exit_code() {
        let out = run(["/bin/sh", "-c", "printf hello; exit 3"]).unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, b"hello");
        assert!(!out.truncated);
    }

    #[test]
    fn environment_is_not_inherited_by_default() {
        std::env::set_var("CI_RS_SHOULD_NOT_LEAK", "1");
        let out = run(["/usr/bin/env"]).unwrap();
        assert!(!out.stdout_lossy().contains("CI_RS_SHOULD_NOT_LEAK"));
    }

    #[test]
    fn kills_on_timeout() {
        let err = Subp::new(["/bin/sh", "-c", "sleep 30"])
            .timeout(Some(Duration::from_millis(150)))
            .run()
            .unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
    }

    #[test]
    fn truncates_runaway_output() {
        let out = Subp::new(["/bin/sh", "-c", "yes ci-rs | head -c 200000"])
            .max_output(1024)
            .run()
            .unwrap();
        assert_eq!(out.stdout.len(), 1024);
        assert!(out.truncated);
    }

    #[test]
    fn feeds_stdin() {
        let out = Subp::new(["/bin/cat"]).stdin("piped").run().unwrap();
        assert_eq!(out.stdout, b"piped");
    }

    #[test]
    fn passthrough_reports_the_status_without_capturing() {
        let status = Subp::new(["/bin/sh", "-c", "exit 7"])
            .passthrough()
            .unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn passthrough_honours_the_timeout() {
        let err = Subp::new(["/bin/sh", "-c", "sleep 30"])
            .timeout(Some(Duration::from_millis(150)))
            .passthrough()
            .unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
    }
}
