//! `debug`, `warn` and `error`.
//!
//! The script opens fd 3 on first use and appends everything to it; the
//! destination is `$DI_LOG`, which defaults to `${PATH_RUN_CI}/ds-identify.log`
//! and falls back to stderr if that cannot be opened. `warn` and `error` go to
//! both the log and stderr.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
enum Sink {
    /// `DI_LOG` empty: `debug` substitutes "stderr" on first use.
    Unset,
    Stderr,
    File(Box<File>),
    /// Named but not opened yet; the script opens lazily, on the first message.
    Pending(PathBuf),
}

#[derive(Debug)]
pub struct Log {
    level: i32,
    sink: Sink,
}

impl Log {
    /// `DI_LOG` and `DEBUG_LEVEL` from the environment, before `set_run_path`
    /// has had a chance to supply the default log location.
    #[must_use]
    pub fn from_env() -> Self {
        let level = std::env::var("DEBUG_LEVEL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let sink = match std::env::var("DI_LOG") {
            Ok(path) if path == "stderr" => Sink::Stderr,
            Ok(path) if !path.is_empty() => Sink::Pending(PathBuf::from(path)),
            _ => Sink::Unset,
        };
        Self { level, sink }
    }

    /// True when `DI_LOG` was left unset or empty, so `set_run_path` owns it.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        matches!(self.sink, Sink::Unset)
    }

    pub fn set_path(&mut self, path: &Path) {
        self.sink = Sink::Pending(path.to_path_buf());
    }

    #[must_use]
    pub fn is_stderr(&self) -> bool {
        matches!(self.sink, Sink::Stderr | Sink::Unset)
    }

    fn open(&mut self) {
        let Sink::Pending(path) = &self.sink else {
            return;
        };
        let path = path.clone();
        if let Some(parent) = path.parent() {
            if !parent.is_dir() && std::fs::create_dir_all(parent).is_err() {
                eprintln!("ERROR: cannot write to {}", path.display());
                self.sink = Sink::Stderr;
                return;
            }
        }
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
            self.sink = Sink::File(Box::new(file));
        } else {
            eprintln!(
                "ERROR: failed writing to {}. logging to stderr.",
                path.display()
            );
            self.sink = Sink::Stderr;
        }
    }

    /// Writes one line to the log destination, whatever it currently is.
    pub fn write(&mut self, line: &str) {
        self.open();
        match &mut self.sink {
            Sink::Unset | Sink::Stderr | Sink::Pending(_) => eprintln!("{line}"),
            Sink::File(file) => {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    pub fn debug(&mut self, level: i32, message: &str) {
        if level > self.level {
            return;
        }
        self.write(message);
    }

    pub fn warn(&mut self, message: &str) {
        let line = format!("WARN: {message}");
        self.debug(0, &line);
        eprintln!("{line}");
    }

    pub fn error(&mut self, message: &str) {
        let line = format!("ERROR: {message}");
        self.debug(0, &line);
        eprintln!("{line}");
    }
}
