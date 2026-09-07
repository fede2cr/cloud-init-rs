//! Port of `cc_rsyslog.py`: drop config files into `/etc/rsyslog.d` and ask
//! the service to reload.
//!
//! The machine goes behind [`Host`] for the same reason `cc_growpart` does:
//! the module interleaves deciding and doing -- whether to install the package
//! depends on `which(check_exe)`, whether to say anything at the end depends
//! on whether the reload worked -- so the differential compares the ordered
//! list of things it did as well as the log.
//!
//! Two whole branches of upstream are unreachable here and are ported as
//! written rather than dropped:
//!
//! - `DISTRO_OVERRIDES` is keyed on `distro.osfamily`, and the three BSD
//!   classes set that from `platform.system().lower()`. On Linux that is
//!   `linux` for every distro, so the override never fires -- for the packaged
//!   Python on this machine exactly as much as for the port.
//! - `util.is_BSD()` is the same test, so the `enable`/`disable syslogd` block
//!   in `handle` is dead too.

use ci_config::{repr, type_name, Object, Value};
use ci_log::Logger;

use super::growpart::{indent_text, logexc, py_list};
use super::{py_str, Args};

const SOURCE: &str = "cc_rsyslog.py";

/// `RSYSLOG_CONFIG`, which is also every distro's config on Linux.
const CONFIG_DIR: &str = "/etc/rsyslog.d";
const CONFIG_FILENAME: &str = "20-cloud-config.conf";
const SERVICE_RELOAD_COMMAND: &str = "auto";
const CHECK_EXE: &str = "rsyslogd";
const PACKAGES: [&str; 1] = ["rsyslog"];

/// `service_reload_command`, handed to `subp.subp` as it stands.
///
/// A list is an argv, element types included: upstream converts nothing, so a
/// number reaches `Popen` as a number. A string is *not* split -- `subp` wraps
/// it in a one-element list, so `"systemctl restart rsyslog"` looks for a
/// program of that whole name (bug B90).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Line(String),
    Argv(Vec<Value>),
}

impl Command {
    /// `str(cmd)` as the `Command:` line of a `ProcessExecutionError` shows
    /// it, which is also the key a [`Fixture`] scripts it under.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Line(line) => line.clone(),
            Self::Argv(argv) => repr(&Value::Array(argv.clone())),
        }
    }
}

/// A `subp` failure, whose `Command:` line is `str(cmd)` and so depends on
/// whether the command was given as a list or as one string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdError {
    pub command: String,
    /// `None` for a child that never got an exit code, printed as `-`.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Unexpected error while running command.\n\
             Command: {}\n\
             Exit code: {}\n\
             Reason: -\n\
             Stdout: {}\n\
             Stderr: {}",
            self.command,
            self.exit_code
                .map_or_else(|| "-".to_owned(), |code| code.to_string()),
            indent_text(&self.stdout),
            indent_text(&self.stderr),
        )
    }
}

/// Everything `cc_rsyslog` asks of the machine.
///
/// `&mut self` is for recording rather than state: a [`Fixture`] appends each
/// call to a list so the differential compares the sequence.
pub trait Host {
    /// `subp.which(program)`.
    fn which(&mut self, program: &str) -> bool;

    /// `cloud.distro.install_packages(packages)`.
    fn install_packages(&mut self, packages: &Value) -> Result<(), String>;

    /// `cloud.distro.manage_service(action, service)`.
    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError>;

    /// `subp.subp(command, capture=True)`.
    fn subp(&mut self, command: &Command) -> Result<(), CmdError>;

    /// `util.write_file(path, content, omode=)`, truncating or appending.
    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        append: bool,
    ) -> Result<(), String>;

    /// `loggers.reset_logging()` then `loggers.setup_logging(cloud.cfg)`,
    /// which only happens when the reload actually worked.
    fn reset_logging(&mut self);
}

/// `load_config`'s return: `mycfg` with every key filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub configs: Vec<Value>,
    pub config_dir: String,
    pub config_filename: String,
    pub remotes: Object,
    pub service_reload_command: Command,
    pub check_exe: String,
    pub packages: Value,
    pub install_rsyslog: bool,
}

/// `load_config`.
///
/// Upstream fills the defaults into `cfg["rsyslog"]` itself, so a later module
/// sees the completed dict; the port works on a copy (deviation 160).
///
/// # Errors
/// The `ValueError` a wrongly-typed key raises, and the `TypeError` a
/// `rsyslog:` that is not a mapping raises on the first `in` test.
pub fn load_config(cfg: &Object, log: &mut Logger) -> Result<Loaded, String> {
    let raw = cfg.get("rsyslog");
    let mut mycfg = match raw {
        Some(Value::Object(map)) => map.clone(),
        None => Object::new(),
        Some(Value::Array(items)) => {
            deprecate(log, "The rsyslog key with value of type 'list'");
            let mut map = Object::new();
            map.insert("configs".to_owned(), Value::Array(items.clone()));
            if let Some(filename) = cfg.get("rsyslog_filename") {
                map.insert("config_filename".to_owned(), filename.clone());
            }
            if let Some(dir) = cfg.get("rsyslog_dir") {
                map.insert("config_dir".to_owned(), dir.clone());
            }
            map
        }
        // `"configs" not in mycfg` is the first thing the fillup loop does,
        // and `in` on a string is a substring test that succeeds, so a string
        // gets one line further than the rest before it dies.
        Some(Value::String(_)) => {
            return Err("'str' object does not support item assignment".to_owned())
        }
        Some(other) => {
            return Err(format!(
                "argument of type '{}' is not a container or iterable",
                type_name(other)
            ))
        }
    };

    // `fillup`, in order: the message names the type that was expected, and
    // `type()` of what was there.
    let configs = take(&mut mycfg, "configs", "<class 'list'>", |value| {
        matches!(value, Value::Array(_))
    })?;
    let config_dir = take(&mut mycfg, "config_dir", "<class 'str'>", |value| {
        matches!(value, Value::String(_))
    })?;
    let config_filename =
        take(&mut mycfg, "config_filename", "<class 'str'>", |value| {
            matches!(value, Value::String(_))
        })?;
    let remotes = take(&mut mycfg, "remotes", "<class 'dict'>", |value| {
        matches!(value, Value::Object(_))
    })?;
    let reload = take(
        &mut mycfg,
        "service_reload_command",
        "(<class 'str'>, <class 'list'>)",
        |value| matches!(value, Value::String(_) | Value::Array(_)),
    )?;
    let check_exe = take(&mut mycfg, "check_exe", "<class 'str'>", |value| {
        matches!(value, Value::String(_))
    })?;
    let packages = take(&mut mycfg, "packages", "<class 'list'>", |value| {
        matches!(value, Value::Array(_))
    })?;
    let install = take(
        &mut mycfg,
        "install_rsyslog",
        "<class 'bool'>",
        // `isinstance(1, bool)` is False even though `bool` subclasses `int`.
        |value| matches!(value, Value::Bool(_)),
    )?;

    Ok(Loaded {
        configs: match configs {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        },
        config_dir: string_or(config_dir, CONFIG_DIR),
        config_filename: string_or(config_filename, CONFIG_FILENAME),
        remotes: match remotes {
            Some(Value::Object(map)) => map,
            _ => Object::new(),
        },
        service_reload_command: match reload {
            Some(Value::Array(items)) => Command::Argv(items),
            Some(Value::String(text)) => Command::Line(text),
            _ => Command::Line(SERVICE_RELOAD_COMMAND.to_owned()),
        },
        check_exe: string_or(check_exe, CHECK_EXE),
        packages: packages.unwrap_or_else(|| {
            Value::Array(
                PACKAGES
                    .iter()
                    .map(|name| Value::String((*name).to_owned()))
                    .collect(),
            )
        }),
        install_rsyslog: matches!(install, Some(Value::Bool(true))),
    })
}

/// One `fillup` row: absent means "use the default", present and wrongly typed
/// is the `ValueError`.
fn take(
    mycfg: &mut Object,
    key: &str,
    expected: &str,
    ok: impl Fn(&Value) -> bool,
) -> Result<Option<Value>, String> {
    match mycfg.shift_remove(key) {
        None => Ok(None),
        Some(value) if ok(&value) => Ok(Some(value)),
        Some(value) => Err(format!(
            "Invalid type for key `{key}`. Expected type(s): {expected}. \
             Current type: <class '{}'>",
            type_name(&value)
        )),
    }
}

fn string_or(value: Option<Value>, default: &str) -> String {
    match value {
        Some(Value::String(text)) => text,
        _ => default.to_owned(),
    }
}

/// An argv element as `subp` receives it: the schema says strings, and
/// anything else reaches `Popen` unconverted.
fn py_token(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => repr(other),
    }
}

/// `apply_rsyslog_changes`: the files that were written, in the order they
/// were first touched.
pub fn apply_rsyslog_changes(
    host: &mut dyn Host,
    configs: &[Value],
    def_fname: &str,
    cfg_dir: &str,
    log: &mut Logger,
) -> Result<Vec<String>, String> {
    let mut files: Vec<String> = Vec::new();
    for (index, entry) in configs.iter().enumerate() {
        let position = index + 1;
        let (content, filename) = match entry {
            Value::Object(map) => {
                let Some(content) = map.get("content") else {
                    log.warning(
                        SOURCE,
                        &format!("No 'content' entry in config entry {position}"),
                    );
                    continue;
                };
                let filename = map.get("filename").cloned();
                (content.clone(), filename)
            }
            other => (other.clone(), None),
        };
        let filename = filename.unwrap_or_else(|| Value::String(def_fname.to_owned()));

        // `.strip()` is outside the try, so a non-string filename ends the
        // module rather than the entry.
        let Value::String(filename) = filename else {
            return Err(format!(
                "'{}' object has no attribute 'strip'",
                type_name(&filename)
            ));
        };
        let filename = filename.trim_matches(is_py_space);
        if filename.is_empty() {
            log.warning(SOURCE, &format!("Entry {position} has an empty filename"));
            continue;
        }

        let path = join(cfg_dir, filename);
        let append = files.iter().any(|seen| seen == &path);
        if !append {
            files.push(path.clone());
        }

        // Everything from here is inside upstream's `try`, including the
        // `.endswith` that a non-string content trips over.
        let Value::String(content) = &content else {
            logexc(log, &format!("Failed to write to {path}"));
            continue;
        };
        let endl = if content.ends_with('\n') { "" } else { "\n" };
        if host
            .write_file(&path, &format!("{content}{endl}"), append)
            .is_err()
        {
            logexc(log, &format!("Failed to write to {path}"));
        }
    }
    Ok(files)
}

/// `os.path.join(cfg_dir, filename)`.
fn join(dir: &str, filename: &str) -> String {
    if filename.starts_with('/') {
        return filename.to_owned();
    }
    if dir.is_empty() || dir.ends_with('/') {
        format!("{dir}{filename}")
    } else {
        format!("{dir}/{filename}")
    }
}

/// `str.strip()` with no argument, which is Unicode whitespace.
fn is_py_space(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}' | '\u{85}')
}

/// One parsed `remotes` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotesLine {
    pub name: Option<String>,
    pub match_: String,
    pub proto: String,
    pub addr: String,
    /// `int(port)`, which the regex has already restricted to digits.
    pub port: Option<i128>,
}

impl std::fmt::Display for RemotesLine {
    /// `SyslogRemotesLine.__str__`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ", self.match_)?;
        match self.proto.as_str() {
            "udp" => write!(f, "@")?,
            "tcp" => write!(f, "@@")?,
            _ => {}
        }
        if self.addr.contains(':') {
            write!(f, "[{}]", self.addr)?;
        } else {
            write!(f, "{}", self.addr)?;
        }
        // `if self.port:` -- so a `:0` is parsed and then dropped (bug B91).
        if self.port.is_some_and(|port| port != 0) {
            write!(f, ":{}", self.port.unwrap_or_default())?;
        }
        if let Some(name) = &self.name {
            write!(f, " # {name}")?;
        }
        Ok(())
    }
}

/// Why a `remotes` line could not be parsed.
///
/// The distinction matters: `remotes_to_rsyslog_cfg` catches `ValueError` and
/// warns, but the `AttributeError` a bare port reaches instead of the
/// "address is required" check goes straight out of `handle` (bug B92).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteError {
    pub message: String,
    /// `True` for the `AttributeError`, which nothing catches.
    pub fatal: bool,
}

impl RemoteError {
    fn value(message: String) -> Self {
        Self {
            message,
            fatal: false,
        }
    }
}

/// `parse_remotes_line`.
///
/// # Errors
/// Every `ValueError` upstream raises, plus the `AttributeError` it reaches
/// instead of "address is required" when the host half is a bare port.
pub fn parse_remotes_line(
    line: &str,
    name: Option<&str>,
) -> Result<RemotesLine, RemoteError> {
    // `re.split(r"[ ]*[#]+[ ]*", line)`, unpacked into exactly two names: any
    // other number of pieces is the `ValueError` that leaves the line whole.
    let pieces = split_comment(line);
    let (data, comment) = if pieces.len() == 2 {
        (
            pieces.first().map_or("", String::as_str).to_owned(),
            Some(pieces.get(1).map_or("", String::as_str).trim().to_owned()),
        )
    } else {
        (line.to_owned(), None)
    };

    let toks: Vec<&str> = data.split_whitespace().collect();
    let (match_, host_port) = match toks.len() {
        // `host_port = data`, not `data.strip()`: upstream drops the strip on
        // this branch only, and the regex tolerates the spaces it leaves.
        1 => (None, data.clone()),
        2 => (
            Some(toks.first().map_or("", |tok| *tok).to_owned()),
            toks.get(1).map_or("", |tok| *tok).to_owned(),
        ),
        _ => {
            return Err(RemoteError::value(format!(
                "line had multiple spaces: {data}"
            )))
        }
    };

    let Some((proto, addr, port)) = split_host_port(&host_port) else {
        return Err(RemoteError::value(format!(
            "Invalid host specification '{host_port}'"
        )));
    };

    // `toks.group("addr") or toks.group("bracket_addr")`: an empty `addr` and
    // an absent `bracket_addr` leave `None`, and `None.startswith` is what
    // runs instead of the "address is required" check below.
    let addr = match addr {
        Some(found) if !found.is_empty() => found,
        // The bracket alternative matched, so `bracket_addr` is a string --
        // possibly the empty one, which does reach `validate`.
        None => bracket_addr(&host_port).unwrap_or_default(),
        Some(_) => {
            return Err(RemoteError {
                message: "'NoneType' object has no attribute 'startswith'".to_owned(),
                fatal: true,
            })
        }
    };

    if addr.starts_with('[') && !addr.ends_with(']') {
        return Err(RemoteError::value(format!(
            "host spec had invalid brackets: {addr}"
        )));
    }

    let name = match (name, comment) {
        (None, Some(comment)) if !comment.is_empty() => Some(comment),
        (name, _) => name.map(str::to_owned),
    };

    let line = RemotesLine {
        name,
        match_: match_.unwrap_or_else(|| "*.*".to_owned()),
        proto: match proto.as_str() {
            "@@" => "tcp".to_owned(),
            _ => "udp".to_owned(),
        },
        addr,
        port: port.map(|digits| digits.parse().unwrap_or(i128::MAX)),
    };
    if line.addr.is_empty() {
        return Err(RemoteError::value("address is required".to_owned()));
    }
    Ok(line)
}

/// `re.split(r"[ ]*[#]+[ ]*", line)`.
fn split_comment(line: &str) -> Vec<String> {
    let bytes: Vec<char> = line.chars().collect();
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut index = 0;
    while index < bytes.len() {
        // A separator is any run of spaces, then at least one `#`, then any
        // run of spaces.
        let start = index;
        let mut cursor = index;
        while bytes.get(cursor) == Some(&' ') {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&'#') {
            while bytes.get(cursor) == Some(&'#') {
                cursor += 1;
            }
            while bytes.get(cursor) == Some(&' ') {
                cursor += 1;
            }
            pieces.push(std::mem::take(&mut current));
            index = cursor;
            continue;
        }
        index = start;
        if let Some(ch) = bytes.get(index) {
            current.push(*ch);
        }
        index += 1;
    }
    pieces.push(current);
    pieces
}

/// The host regex, which is three optional pieces and nothing else:
/// `^(@{0,2})((\[([^\]]*)\])|([^:]*))(:([0-9]+))?$`.
///
/// Returns the proto, the unbracketed address group (`None` when the bracket
/// alternative matched) and the port digits.
fn split_host_port(text: &str) -> Option<(String, Option<String>, Option<String>)> {
    let rest = text.strip_prefix("@@").map_or_else(
        || {
            text.strip_prefix('@')
                .map_or((String::new(), text), |rest| ("@".to_owned(), rest))
        },
        |rest| ("@@".to_owned(), rest),
    );
    let (proto, rest) = rest;

    // The port suffix is greedy-optional: the regex only takes it when what is
    // left of it satisfies one of the two address alternatives, and the
    // bracket one is tried first.
    for (body, port) in port_splits(rest) {
        if let Some(inner) = body
            .strip_prefix('[')
            .and_then(|body| body.strip_suffix(']'))
        {
            if !inner.contains(']') {
                return Some((proto, None, port));
            }
        }
        if !body.contains(':') {
            return Some((proto, Some(body.to_owned()), port));
        }
    }
    None
}

/// The two ways the regex can cut a trailing `:<digits>` off, longest first.
fn port_splits(text: &str) -> Vec<(&str, Option<String>)> {
    let mut out = Vec::new();
    if let Some(colon) = text.rfind(':') {
        let digits = text.get(colon + 1..).unwrap_or_default();
        if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            out.push((
                text.get(..colon).unwrap_or_default(),
                Some(digits.to_owned()),
            ));
        }
    }
    out.push((text, None));
    out
}

/// The `bracket_addr` group of a host spec whose bracket alternative matched.
fn bracket_addr(host_port: &str) -> Option<String> {
    let open = host_port.find('[')?;
    let inner = host_port.get(open + 1..)?;
    let close = inner.find(']')?;
    inner.get(..close).map(str::to_owned)
}

/// `remotes_to_rsyslog_cfg`.
///
/// # Errors
/// The `AttributeError` from [`parse_remotes_line`], which the `except
/// ValueError` around the call does not catch.
pub fn remotes_to_rsyslog_cfg(
    remotes: &Object,
    header: Option<&str>,
    footer: Option<&str>,
    log: &mut Logger,
) -> Result<Option<String>, String> {
    if remotes.is_empty() {
        return Ok(None);
    }
    let mut lines: Vec<String> = Vec::new();
    if let Some(header) = header {
        lines.push(header.to_owned());
    }
    for (name, value) in remotes {
        // `if not line: continue` -- every falsy value, not just the empty
        // string.
        if !py_truthy(value) {
            continue;
        }
        let text = match value {
            Value::String(text) => text.clone(),
            // A non-string reaches `re.split` and dies there; upstream only
            // catches `ValueError`, so this is not survivable.
            other => {
                return Err(format!(
                    "expected string or bytes-like object, got '{}'",
                    type_name(other)
                ))
            }
        };
        match parse_remotes_line(&text, Some(name)) {
            Ok(parsed) => lines.push(parsed.to_string()),
            Err(error) if error.fatal => return Err(error.message),
            Err(error) => log.warning(
                SOURCE,
                &format!("failed loading remote {name}: {text} [{}]", error.message),
            ),
        }
    }
    if let Some(footer) = footer {
        lines.push(footer.to_owned());
    }
    Ok(Some(format!("{}\n", lines.join("\n"))))
}

/// `install_rsyslog`.
fn install_rsyslog(
    host: &mut dyn Host,
    packages: &Value,
    check_exe: &str,
) -> Result<(), String> {
    if host.which(check_exe) {
        return Ok(());
    }
    host.install_packages(packages)
}

/// `reload_syslog`.
fn reload_syslog(
    host: &mut dyn Host,
    command: &Command,
    service: &str,
) -> Result<(), CmdError> {
    match command {
        Command::Line(line) if line == SERVICE_RELOAD_COMMAND => {
            host.manage_service("try-reload", service)
        }
        other => host.subp(other),
    }
}

/// `handle`, against a scripted machine.
///
/// # Errors
/// Whatever `load_config` and `apply_rsyslog_changes` let escape.
pub fn handle_with(
    name: &str,
    cfg: &Object,
    system_info: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    if !cfg.contains_key("rsyslog") {
        log.debug(
            SOURCE,
            &format!(
                "Skipping module named {name}, no 'rsyslog' key in \
                 configuration"
            ),
        );
        return Ok(());
    }

    let mycfg = load_config(cfg, log)?;
    let mut configs = mycfg.configs.clone();

    if !mycfg.remotes.is_empty() {
        if let Some(text) = remotes_to_rsyslog_cfg(
            &mycfg.remotes,
            Some("# begin remotes"),
            Some("# end remotes"),
            log,
        )? {
            configs.push(Value::String(text));
        }
    }

    // `distro.get_option("rsyslog_svcname", "rsyslog")`, which returns the
    // value as configured -- a number reaches the argv as `str()` of itself.
    // Upstream reads it here for the BSD block that Linux never enters, and
    // again inside `reload_syslog`.
    let service = system_info
        .get("rsyslog_svcname")
        .map_or_else(|| "rsyslog".to_owned(), py_str);

    if mycfg.install_rsyslog {
        install_rsyslog(host, &mycfg.packages, &mycfg.check_exe)?;
    }

    if configs.is_empty() {
        log.debug(SOURCE, "Empty config rsyslog['configs'], nothing to do");
        return Ok(());
    }

    let changes = apply_rsyslog_changes(
        host,
        &configs,
        &mycfg.config_filename,
        &mycfg.config_dir,
        log,
    )?;

    if changes.is_empty() {
        log.debug(SOURCE, "restart of syslog not necessary, no changes made");
        return Ok(());
    }

    let restarted = match reload_syslog(host, &mycfg.service_reload_command, &service) {
        Ok(()) => true,
        Err(error) => {
            log.warning(SOURCE, &format!("Failed to reload syslog {error}"));
            false
        }
    };

    if restarted {
        host.reset_logging();
        log.debug(
            SOURCE,
            &format!("{name} configured {} files", py_list(&changes)),
        );
    }
    Ok(())
}

/// The registry entry point.
///
/// # Panics
/// Never; the `Result` is turned into the module failure the stage reports.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let mut host = Live {
        root: args.root.to_owned(),
        distro: *args.distro,
        system_info: args.system_info.clone(),
    };
    let (name, cfg, system_info) = (
        args.name.to_owned(),
        args.cfg.clone(),
        args.system_info.clone(),
    );
    handle_with(&name, &cfg, &system_info, &mut host, &mut *args.logger)
}

/// `lifecycle.deprecate(deprecated_version="22.2")`, whose message is
/// `.rstrip()`ed and so has no trailing space when there is no remedy to
/// suggest.
fn deprecate(log: &mut Logger, deprecated: &str) {
    log.log(
        ci_log::Level::Deprecated,
        "lifecycle.py",
        &format!(
            "{deprecated} is deprecated in 22.2 and scheduled to be removed \
             in 27.2."
        ),
    );
}

/// `util.is_true`-style truthiness, which is Python's.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// The real machine.
#[derive(Debug, Clone)]
pub struct Live {
    root: std::path::PathBuf,
    distro: ci_distro::Distro,
    system_info: Object,
}

/// `cloud.distro.install_packages(pkgs)` against the real machine, shared with
/// `cc_puppet`.
///
/// # Errors
/// A package manager that could not be configured or run, or a package the
/// plan could not place.
pub(crate) fn install_packages(
    root: &std::path::Path,
    distro: &ci_distro::Distro,
    system_info: &Object,
    packages: &Value,
) -> Result<(), String> {
    let items = match packages {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    let managers = distro.package_managers;
    let state = super::package_update_upgrade_install::State::probe(root, managers);
    let config =
        ci_distro::packages::AptConfig::from_config(system_info, &mut |name| {
            ci_sys::subp::which(name).is_some()
        })?;
    let mut quiet = Logger::silent();
    let plan = ci_distro::packages::plan_install(
        managers,
        &config,
        &state.packages,
        &items,
        &mut quiet,
    )?;
    ci_distro::packages::run(&plan.steps, &mut |step| {
        let mut command = ci_sys::subp::Subp::new(step.argv.clone()).inherit_env();
        for (key, value) in &step.env {
            command = command.env(key, value);
        }
        command.check().map(|_| ()).map_err(|e| e.to_string())
    })?;
    plan.failed.map_or(Ok(()), Err)
}

impl Host for Live {
    fn which(&mut self, program: &str) -> bool {
        ci_sys::subp::which(program).is_some()
    }

    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        install_packages(&self.root, &self.distro, &self.system_info, packages)
    }

    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError> {
        let argv = ci_distro::service::command(&self.distro, action, service, &[])
            .map_err(|key| CmdError {
                command: key,
                ..CmdError::default()
            })?;
        self.subp(&Command::Argv(
            argv.into_iter().map(Value::String).collect(),
        ))
    }

    fn subp(&mut self, command: &Command) -> Result<(), CmdError> {
        let argv = match command {
            Command::Line(line) => vec![line.clone()],
            Command::Argv(argv) => argv.iter().map(py_token).collect(),
        };
        let out = ci_sys::subp::Subp::new(&argv)
            .run()
            .map_err(|error| CmdError {
                command: command.text(),
                exit_code: None,
                stdout: String::new(),
                stderr: error.to_string(),
            })?;
        if out.code == Some(0) {
            return Ok(());
        }
        Err(CmdError {
            command: command.text(),
            exit_code: out.code,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        append: bool,
    ) -> Result<(), String> {
        let target = self.root.join(path.trim_start_matches('/'));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if append {
            ci_sys::atomic::append_file(&target, content.as_bytes(), 0o644)
                .map_err(|error| error.to_string())
        } else {
            ci_sys::atomic::write_file(
                &target,
                content.as_bytes(),
                ci_sys::atomic::WriteOptions::default(),
            )
            .map_err(|error| error.to_string())
        }
    }

    fn reset_logging(&mut self) {
        // `loggers.reset_logging()` tears down Python's handlers so that the
        // rsyslog one just written takes effect. The port's logger owns its
        // sinks and has nothing to tear down; the `configured N files` line
        // that follows is what the differential compares.
    }
}

/// A [`Host`] whose every answer is set up front, for the differential.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// `which` answers, by program name.
    pub present: Vec<String>,
    /// Failures, keyed by the call as it is recorded.
    pub failures: Vec<(String, CmdError)>,
    /// Paths `write_file` refuses, and the error it gives.
    pub unwritable: Vec<(String, String)>,
    pub calls: Vec<String>,
    /// What each file ended up holding, in the order they were opened.
    pub written: Vec<(String, String)>,
}

impl Fixture {
    fn record(&mut self, call: String) {
        self.calls.push(call);
    }

    /// The scripted failure for a call, if there is one. A case that does not
    /// say what `str(cmd)` was gets `default`, which is what the Python driver
    /// falls back to as well.
    fn failure(&self, call: &str, default: &str) -> Option<CmdError> {
        self.failures
            .iter()
            .find(|(key, _)| key == call)
            .map(|(_, error)| {
                let mut error = error.clone();
                if error.command.is_empty() {
                    default.clone_into(&mut error.command);
                }
                error
            })
    }
}

impl Host for Fixture {
    fn which(&mut self, program: &str) -> bool {
        self.record(format!("which {program}"));
        self.present.iter().any(|name| name == program)
    }

    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        let call = format!("install_packages {}", repr(packages));
        self.record(call.clone());
        self.failure(&call, &call)
            .map_or(Ok(()), |error| Err(error.to_string()))
    }

    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError> {
        let call = format!("manage_service {action} {service}");
        self.record(call.clone());
        self.failure(&call, &call).map_or(Ok(()), Err)
    }

    fn subp(&mut self, command: &Command) -> Result<(), CmdError> {
        let text = command.text();
        let call = format!("subp {text}");
        self.record(call.clone());
        self.failure(&call, &text).map_or(Ok(()), Err)
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        append: bool,
    ) -> Result<(), String> {
        self.record(format!(
            "write_file {path} {}",
            if append { "ab" } else { "wb" }
        ));
        if let Some((_, error)) = self.unwritable.iter().find(|(key, _)| key == path) {
            return Err(error.clone());
        }
        if let Some(slot) = self.written.iter_mut().find(|(key, _)| key == path) {
            if append {
                slot.1.push_str(content);
            } else {
                content.clone_into(&mut slot.1);
            }
        } else {
            self.written.push((path.to_owned(), content.to_owned()));
        }
        Ok(())
    }

    fn reset_logging(&mut self) {
        self.record("reset_logging".to_owned());
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    fn run(cfg: &Value, host: &mut Fixture) -> (Vec<String>, Result<(), String>) {
        let mut log = Logger::capturing();
        let cfg = match cfg {
            Value::Object(map) => map.clone(),
            _ => Object::new(),
        };
        let outcome = handle_with("rsyslog", &cfg, &Object::new(), host, &mut log);
        (log.captured().to_vec(), outcome)
    }

    #[test]
    fn a_config_entry_is_written_and_the_service_is_asked_to_reload() {
        let mut host = Fixture::default();
        let (log, outcome) = run(
            &serde_json::json!({"rsyslog": {"configs": ["*.* @@syslog:514"]}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert_eq!(
            host.calls,
            [
                "write_file /etc/rsyslog.d/20-cloud-config.conf wb",
                "manage_service try-reload rsyslog",
                "reset_logging",
            ]
        );
        // The trailing newline is added because the entry did not have one.
        assert_eq!(host.written[0].1, "*.* @@syslog:514\n");
        assert!(log.iter().any(|line| line
            == "cc_rsyslog.py[DEBUG]: rsyslog configured \
                ['/etc/rsyslog.d/20-cloud-config.conf'] files"));
    }

    #[test]
    fn two_entries_naming_one_file_truncate_then_append() {
        let mut host = Fixture::default();
        let (_, outcome) = run(
            &serde_json::json!({"rsyslog": {"configs": [
                {"content": "one", "filename": "a.conf"},
                {"content": "two", "filename": "a.conf"},
            ]}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert_eq!(
            host.calls.first().unwrap(),
            "write_file /etc/rsyslog.d/a.conf wb"
        );
        assert_eq!(
            host.calls.get(1).unwrap(),
            "write_file /etc/rsyslog.d/a.conf ab"
        );
        assert_eq!(host.written[0].1, "one\ntwo\n");
    }

    #[test]
    fn a_reload_that_fails_suppresses_the_closing_line() {
        let mut host = Fixture {
            failures: vec![(
                "manage_service try-reload rsyslog".to_owned(),
                CmdError {
                    command: "['systemctl', 'try-reload-or-restart', 'rsyslog']"
                        .to_owned(),
                    exit_code: Some(5),
                    stdout: String::new(),
                    stderr: "Unit not found.\n".to_owned(),
                },
            )],
            ..Fixture::default()
        };
        let (log, outcome) = run(
            &serde_json::json!({"rsyslog": {"configs": ["x"]}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(!host.calls.iter().any(|call| call == "reset_logging"));
        assert!(log
            .iter()
            .any(|line| line
                .starts_with("cc_rsyslog.py[WARNING]: Failed to reload syslog")));
        assert!(!log.iter().any(|line| line.contains("configured")));
    }

    #[test]
    fn remotes_become_one_appended_config_between_two_markers() {
        let mut host = Fixture::default();
        let (_, outcome) = run(
            &serde_json::json!({"rsyslog": {"remotes": {
                "maas": "@@10.0.0.1:514",
                "juju": "10.0.0.2",
            }}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert_eq!(
            host.written[0].1,
            "# begin remotes\n*.* @@10.0.0.1:514 # maas\n\
             *.* @10.0.0.2 # juju\n# end remotes\n"
        );
    }

    #[test]
    fn a_remote_that_does_not_parse_is_a_warning_and_the_rest_are_kept() {
        let mut host = Fixture::default();
        let (log, outcome) = run(
            &serde_json::json!({"rsyslog": {"remotes": {
                "bad": "a b c",
                "good": "10.0.0.2",
            }}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(log.iter().any(|line| line
            == "cc_rsyslog.py[WARNING]: failed loading remote bad: a b c \
                [line had multiple spaces: a b c]"));
        assert!(host.written[0].1.contains("*.* @10.0.0.2 # good"));
    }

    #[test]
    fn a_bare_port_reaches_the_attribute_error_and_ends_the_module() {
        let mut host = Fixture::default();
        let (_, outcome) = run(
            &serde_json::json!({"rsyslog": {"remotes": {"maas": "@@:514"}}}),
            &mut host,
        );
        assert_eq!(
            outcome,
            Err("'NoneType' object has no attribute 'startswith'".to_owned())
        );
        assert!(host.calls.is_empty());
    }

    #[test]
    fn an_empty_bracketed_address_reaches_the_check_that_was_meant_for_it() {
        assert_eq!(
            parse_remotes_line("[]", None).unwrap_err().message,
            "address is required"
        );
    }

    #[test]
    fn a_port_of_zero_is_parsed_and_then_dropped() {
        let parsed = parse_remotes_line("host:0", Some("n")).unwrap();
        assert_eq!(parsed.port, Some(0));
        assert_eq!(parsed.to_string(), "*.* @host # n");
    }

    #[test]
    fn the_comment_names_the_remote_only_when_nothing_else_did() {
        assert_eq!(
            parse_remotes_line("h # from-comment", None).unwrap().name,
            Some("from-comment".to_owned())
        );
        assert_eq!(
            parse_remotes_line("h # from-comment", Some("n"))
                .unwrap()
                .name,
            Some("n".to_owned())
        );
    }

    #[test]
    fn install_only_happens_when_the_check_binary_is_missing() {
        let cfg = serde_json::json!({"rsyslog": {
            "configs": ["x"], "install_rsyslog": true,
        }});

        let mut present = Fixture {
            present: vec!["rsyslogd".to_owned()],
            ..Fixture::default()
        };
        let _ = run(&cfg, &mut present);
        assert_eq!(present.calls.first().unwrap(), "which rsyslogd");
        assert!(!present
            .calls
            .iter()
            .any(|call| call.starts_with("install_packages")));

        let mut absent = Fixture::default();
        let _ = run(&cfg, &mut absent);
        assert_eq!(absent.calls.get(1).unwrap(), "install_packages ['rsyslog']");
    }

    #[test]
    fn a_reload_command_given_as_a_string_is_one_argv_element() {
        let mut host = Fixture::default();
        let _ = run(
            &serde_json::json!({"rsyslog": {
                "configs": ["x"],
                "service_reload_command": "systemctl restart rsyslog",
            }}),
            &mut host,
        );
        assert!(host
            .calls
            .iter()
            .any(|call| call == "subp systemctl restart rsyslog"));
    }

    #[test]
    fn a_wrongly_typed_key_names_the_type_it_wanted_and_the_one_it_got() {
        let mut host = Fixture::default();
        let (_, outcome) = run(
            &serde_json::json!({"rsyslog": {"service_reload_command": 5}}),
            &mut host,
        );
        assert_eq!(
            outcome,
            Err("Invalid type for key `service_reload_command`. Expected \
                 type(s): (<class 'str'>, <class 'list'>). Current type: \
                 <class 'int'>"
                .to_owned())
        );
    }

    #[test]
    fn an_rsyslog_key_that_is_a_string_dies_one_line_later_than_the_rest() {
        let mut host = Fixture::default();
        assert_eq!(
            run(&serde_json::json!({"rsyslog": "hello"}), &mut host).1,
            Err("'str' object does not support item assignment".to_owned())
        );
        assert_eq!(
            run(&serde_json::json!({"rsyslog": 5}), &mut host).1,
            Err("argument of type 'int' is not a container or iterable".to_owned())
        );
    }
}
