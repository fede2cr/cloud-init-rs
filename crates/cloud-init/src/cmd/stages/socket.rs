//! Port of `cloudinit/socket.py`: the synchronisation protocol behind
//! `cloud-init --all-stages`.
//!
//! `cloud-init-main.service` runs one process for the whole boot and lets the
//! four unit files gate each stage: each of them connects to
//! `/run/cloud-init/share/<stage>.sock`, writes `start`, and blocks reading the
//! reply, which is a shell fragment the unit `eval`s to inherit the stage's
//! exit code. The sockets are bound up front, before any stage runs, so a unit
//! that starts early is queued rather than refused.
//!
//! Nothing here is optional if the port is to drop into the packaged unit
//! layout: without it, `cloud-init-main.service` starts and every other unit
//! waits forever.

use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::PathBuf;

use ci_log::Logger;

/// `settings.DEFAULT_RUN_DIR`. Upstream hardcodes it here rather than reading
/// `system_info/paths/run_dir`, so a config that moves `run_dir` still gets its
/// sockets under `/run/cloud-init`.
const DEFAULT_RUN_DIR: &str = "/run/cloud-init";

/// `socket.sd_notify`.
///
/// A no-op when not running under systemd. Failures are reported rather than
/// fatal: upstream lets the `OSError` out and the whole single-process run dies
/// with it, which is a worse outcome than a missed notification.
pub fn sd_notify(message: &str, logger: &mut Logger) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let path = path.to_string_lossy().into_owned();
    if path.is_empty() {
        return;
    }
    logger.info("socket.py", &format!("Sending sd_notify({message})"));
    let sent = if let Some(name) = path.strip_prefix('@') {
        // Upstream writes `socket_path.replace("@", "\0", 1)` and throws the
        // result away, so an abstract socket never connects (COMPAT.md B54).
        // Doing it properly cannot diverge: the alternative is a hard failure.
        send_abstract(name.as_bytes(), message)
    } else if path.starts_with('/') {
        send_path(&path, message)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Unsupported socket type",
        ))
    };
    if let Err(err) = sent {
        logger.warning("socket.py", &format!("sd_notify({message}) failed: {err}"));
    }
}

fn send_path(path: &str, message: &str) -> std::io::Result<()> {
    let sock = UnixDatagram::unbound()?;
    sock.connect(path)?;
    sock.send(message.as_bytes())?;
    Ok(())
}

fn send_abstract(name: &[u8], message: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
    let sock = UnixDatagram::unbound()?;
    sock.connect_addr(&addr)?;
    sock.send(message.as_bytes())?;
    Ok(())
}

/// `socket.SocketSync`: one listener per stage, all bound at construction.
pub struct SocketSync {
    listeners: Vec<(&'static str, UnixListener)>,
    /// Set when any stage returned non-zero or failed its handshake.
    pub experienced_any_error: bool,
    /// The first handshake failure, for the systemd status line.
    first_exception: Option<String>,
    /// Whether the protocol is in force at all. Upstream skips it when stdin is
    /// a tty, so that `cloud-init --all-stages` typed at a shell still runs.
    interactive: bool,
}

impl SocketSync {
    /// Binds a socket for each stage, removing any stale one first.
    pub fn bind(names: &[&'static str]) -> std::io::Result<Self> {
        let dir = PathBuf::from(DEFAULT_RUN_DIR).join("share");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut listeners = Vec::new();
        for name in names {
            let path = dir.join(format!("{name}.sock"));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            listeners.push((*name, UnixListener::bind(&path)?));
        }
        Ok(Self {
            listeners,
            experienced_any_error: false,
            first_exception: None,
            interactive: std::io::stdin().is_terminal(),
        })
    }

    fn listener(&self, stage: &str) -> Option<&UnixListener> {
        self.listeners
            .iter()
            .find(|(name, _)| *name == stage)
            .map(|(_, sock)| sock)
    }

    /// Waits for `start` on the stage's socket, runs `body`, then reports the
    /// exit code back over the same connection.
    pub fn stage(
        &mut self,
        stage: &'static str,
        logger: &mut Logger,
        body: impl FnOnce(&mut Logger) -> u8,
    ) -> u8 {
        if self.interactive {
            logger.info(
                "socket.py",
                "Stdin is a tty, so skipping stage synchronization protocol",
            );
            let code = body(logger);
            // Upstream's `__enter__` and `__exit__` both return early on a
            // tty, so nothing records the exit code and the run always ends
            // 0 no matter what the stages did.
            return code;
        }

        sd_notify(
            &format!(
                "STATUS=Waiting on external services to complete before \
                 starting the {stage} stage."
            ),
            logger,
        );
        let connection = match self.accept(stage) {
            Ok(connection) => connection,
            Err(why) => {
                // Upstream raises here and `all_stages` stops; the remaining
                // stages never run and the units block on their own sockets.
                self.fail(&why, logger);
                return 1;
            }
        };
        sd_notify(&format!("STATUS=Running ({stage} stage)"), logger);

        let code = body(logger);
        if code != 0 {
            self.experienced_any_error = true;
        }
        reply(
            connection,
            &format!("Completed socket interaction for boot stage {stage}"),
            code,
            logger,
        );
        code
    }

    fn accept(&self, stage: &str) -> Result<UnixStream, String> {
        let listener = self
            .listener(stage)
            .ok_or_else(|| format!("Invalid stage name: {stage}"))?;
        let (mut connection, _) = listener
            .accept()
            .map_err(|err| format!("Failed to accept on the {stage} socket: {err}"))?;
        // The protocol sends exactly "start"; anything else is a caller that
        // does not speak it.
        let mut chunk = [0u8; 5];
        let read = connection
            .read(&mut chunk)
            .map_err(|err| format!("Failed reading the {stage} socket: {err}"))?;
        if chunk.get(..read) != Some(b"start") {
            return Err(format!(
                "Received invalid message: [{}]",
                String::from_utf8_lossy(chunk.get(..read).unwrap_or_default())
            ));
        }
        Ok(connection)
    }

    fn fail(&mut self, why: &str, logger: &mut Logger) {
        self.experienced_any_error = true;
        if self.first_exception.is_none() {
            self.first_exception = Some(why.to_owned());
        }
        logger.error("socket.py", why);
        sd_notify(&format!("STATUS={why}"), logger);
    }

    /// The final `sd_notify` pair, and the process exit code.
    pub fn finish(&mut self, logger: &mut Logger) -> u8 {
        if self.experienced_any_error {
            let message = self.first_exception.clone().map_or_else(
                || "a stage of cloud-init exited non-zero".to_owned(),
                |why| format!("first exception received: {why}"),
            );
            sd_notify(
                &format!(
                    "STATUS=Completed with failure, {message}. Run 'cloud-init \
                     status --long' for more details."
                ),
                logger,
            );
            sd_notify("STOPPING=1", logger);
            return 1;
        }
        sd_notify("STATUS=Completed", logger);
        sd_notify("STOPPING=1", logger);
        0
    }
}

/// The reply is `eval`d by the waiting unit, so it is hardcoded rather than
/// built from anything an instance could influence.
fn reply(mut connection: UnixStream, message: &str, code: u8, logger: &mut Logger) {
    let payload = format!("echo '{message}'; exit {code};");
    if let Err(err) = connection.write_all(payload.as_bytes()) {
        logger.warning("socket.py", &format!("Failed to reply: {err}"));
    }
}
