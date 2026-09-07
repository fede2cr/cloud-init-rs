//! Port of `cc_puppet.py`: install puppet, rewrite `puppet.conf`, enable and
//! run the agent.
//!
//! The module interleaves deciding and doing more than most -- which package
//! it installs depends on which install attempt stopped raising, and what it
//! writes to `puppet.conf` depends on what was already there -- so the machine
//! goes behind [`Host`] and the differential compares the ordered call list as
//! well as the log and the bytes.
//!
//! Four upstream behaviours are reproduced rather than corrected, because a
//! machine that has been through the Python module is in the state they
//! produce (bugs B93 to B96 in `docs/COMPAT.md`):
//!
//! - the rename-and-write pair sits *inside* the per-section loop, so
//!   `puppet.conf.old` is only the operator's original file when `conf:` has
//!   exactly one section;
//! - the existing file is "cleaned" by left-stripping every line, which turns
//!   a continuation line into a bare word and makes the parse fail;
//! - a `conf:` section named `default` in lower case raises `NoSectionError`
//!   where `DEFAULT` works;
//! - `puppet config print` runs three times even when all three paths it
//!   answers for are set in cloud-config, because Python evaluates the default
//!   argument before it knows whether it is needed.

use ci_config::configparser::RawConfigParser;
use ci_config::option::get_bool;
use ci_config::{Object, Value};
use ci_log::Logger;

use super::rsyslog::CmdError;
use super::{py_str, Args};

const SOURCE: &str = "cc_puppet.py";

/// `AIO_INSTALL_URL`.
pub const AIO_INSTALL_URL: &str =
    "https://raw.githubusercontent.com/puppetlabs/install-puppet/main/install.sh";

/// `PUPPET_AGENT_DEFAULT_ARGS`.
const AGENT_DEFAULT_ARGS: [&str; 1] = ["--test"];

/// `PUPPET_PACKAGE_NAMES`, tried in order until one installs.
const PACKAGE_NAMES: [&str; 2] = ["puppet-agent", "puppet"];

/// Everything `cc_puppet` asks of the machine.
///
/// `&mut self` is for recording rather than state: a [`Fixture`] appends each
/// call to a list so the differential compares the sequence.
pub trait Host {
    /// `cloud.distro.install_packages(pkgs)`. An `Err` is the
    /// `PackageInstallerError` the no-name loop suppresses.
    fn install_packages(&mut self, packages: &Value) -> Result<(), String>;

    /// `cloud.distro.manage_service(action, service)`.
    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError>;

    /// `subp.subp(argv)`, returning stdout. `capture` is false for the agent
    /// run and the AIO installer, whose output goes straight to the console.
    fn subp(&mut self, argv: &[String], capture: bool) -> Result<String, CmdError>;

    /// `url_helper.readurl(url=url, retries=5).contents`.
    fn readurl(&mut self, url: &str) -> Result<String, String>;

    /// `temp_utils.tempdir(dir=distro.get_tmp_exec_path(), needs_exe=True)`.
    fn tempdir(&mut self) -> Result<String, String>;

    /// `util.load_text_file(path)`.
    fn load_text_file(&mut self, path: &str) -> Result<String, String>;

    /// `util.write_file(path, content, mode=)`.
    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        mode: Option<u32>,
    ) -> Result<(), String>;

    /// `util.ensure_dir(path, mode=)`.
    fn ensure_dir(&mut self, path: &str, mode: Option<u32>) -> Result<(), String>;

    /// `util.chownbyname(path, user, group)`.
    fn chownbyname(
        &mut self,
        path: &str,
        user: &str,
        group: &str,
    ) -> Result<(), String>;

    /// `util.rename(src, dst)`.
    fn rename(&mut self, src: &str, dst: &str) -> Result<(), String>;

    /// `socket.getfqdn()`, for `%f` in `certname`.
    fn getfqdn(&mut self) -> String;

    /// `cloud.get_instance_id()`, for `%i` in `certname`.
    fn instance_id(&mut self) -> String;
}

/// `PuppetConstants`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constants {
    pub conf_path: String,
    pub ssl_dir: String,
    pub ssl_cert_dir: String,
    pub ssl_cert_path: String,
    pub csr_attributes_path: String,
}

impl Constants {
    #[must_use]
    pub fn new(conf_path: &str, ssl_dir: &str, csr_attributes_path: &str) -> Self {
        let ssl_cert_dir = join(ssl_dir, "certs");
        Self {
            conf_path: conf_path.to_owned(),
            ssl_dir: ssl_dir.to_owned(),
            ssl_cert_path: join(&ssl_cert_dir, "ca.pem"),
            ssl_cert_dir,
            csr_attributes_path: csr_attributes_path.to_owned(),
        }
    }
}

/// `os.path.join` for the two-component case: an absolute second component
/// replaces the first outright.
fn join(base: &str, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_owned();
    }
    if base.is_empty() || base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// `_manage_puppet_services`: try each package's unit in turn, warn if none
/// of them answered.
fn manage_puppet_services(host: &mut dyn Host, action: &str, log: &mut Logger) {
    for name in PACKAGE_NAMES {
        if host
            .manage_service(action, &format!("{name}.service"))
            .is_ok()
        {
            return;
        }
    }
    log.warning(
        SOURCE,
        &format!(
            "Could not '{action}' any of the following services: {}",
            PACKAGE_NAMES.join(", ")
        ),
    );
}

/// `get_config_value`: ask puppet where it keeps something.
///
/// # Errors
/// The `ProcessExecutionError` a missing or unhappy puppet binary raises,
/// which upstream lets escape and so does this.
fn get_config_value(
    host: &mut dyn Host,
    puppet_bin: &str,
    setting: &str,
) -> Result<String, CmdError> {
    let argv = [
        puppet_bin.to_owned(),
        "config".to_owned(),
        "print".to_owned(),
        setting.to_owned(),
    ];
    Ok(host.subp(&argv, true)?.trim_end().to_owned())
}

/// `install_puppet_aio`: fetch the one-shot installer and run it.
///
/// # Errors
/// A download failure, a temp directory that could not be made, a write that
/// failed, or a non-zero exit from the installer.
fn install_puppet_aio(
    host: &mut dyn Host,
    url: &str,
    version: Option<&str>,
    collection: Option<&str>,
    cleanup: bool,
) -> Result<(), String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(version) = version {
        args.push("-v".to_owned());
        args.push(version.to_owned());
    }
    if let Some(collection) = collection {
        args.push("-c".to_owned());
        args.push(collection.to_owned());
    }
    if cleanup {
        args.push("--cleanup".to_owned());
    }
    let content = host.readurl(url)?;
    let dir = host.tempdir()?;
    let script = join(&dir, "puppet-install");
    host.write_file(&script, &content, Some(0o700))?;
    let mut command = vec![script];
    command.extend(args);
    host.subp(&command, false)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `handle`, against a scripted machine.
///
/// # Errors
/// Everything upstream lets escape: a puppet binary that will not answer, an
/// unreadable or unparsable `puppet.conf`, a `conf:` section named `default`,
/// a failed write.
#[expect(
    clippy::too_many_lines,
    reason = "upstream's handle is one function and the order of its steps is \
              the thing under test"
)]
pub fn handle_with(
    name: &str,
    cfg: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    let Some(puppet_cfg) = cfg.get("puppet") else {
        log.debug(
            SOURCE,
            &format!("Skipping module named {name}, no 'puppet' configuration found"),
        );
        return Ok(());
    };
    let puppet_cfg = match puppet_cfg {
        Value::Object(map) => map.clone(),
        other => {
            return Err(format!(
                "argument of type '{}' is not a container or iterable",
                ci_config::type_name(other)
            ))
        }
    };

    let install = get_bool(&puppet_cfg, "install", true);
    let version = cfg_str(&puppet_cfg, "version");
    let collection = cfg_str(&puppet_cfg, "collection");
    let install_type =
        cfg_str(&puppet_cfg, "install_type").unwrap_or_else(|| "packages".to_owned());
    let cleanup = get_bool(&puppet_cfg, "cleanup", true);
    let mut run = get_bool(&puppet_cfg, "exec", false);
    let start_puppetd = get_bool(&puppet_cfg, "start_service", true);
    let aio_install_url = cfg_str(&puppet_cfg, "aio_install_url")
        .unwrap_or_else(|| AIO_INSTALL_URL.to_owned());

    // AIO and distro packages use different paths.
    let (puppet_user, puppet_bin, puppet_package) = if install_type == "aio" {
        ("root", "/opt/puppetlabs/bin/puppet", Some("puppet-agent"))
    } else {
        ("puppet", "puppet", None)
    };
    let mut package_name = cfg_str(&puppet_cfg, "package_name")
        .or_else(|| puppet_package.map(ToOwned::to_owned));

    if !install && version.as_ref().is_some_and(|v| !v.is_empty()) {
        log.warning(
            SOURCE,
            "Puppet install set to false but version supplied, doing nothing.",
        );
    } else if install {
        log.debug(
            SOURCE,
            &format!(
                "Attempting to install puppet {} from {install_type}",
                version
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("latest")
            ),
        );
        if install_type == "packages" {
            if package_name.is_none() {
                for puppet_name in PACKAGE_NAMES {
                    if host
                        .install_packages(&to_install(puppet_name, version.as_ref()))
                        .is_ok()
                    {
                        package_name = Some(puppet_name.to_owned());
                        break;
                    }
                }
                if package_name.as_ref().is_none_or(String::is_empty) {
                    log.warning(
                        SOURCE,
                        &format!(
                            "No installable puppet package in any of: {}",
                            PACKAGE_NAMES.join(", ")
                        ),
                    );
                }
            } else if let Some(name) = package_name.as_deref() {
                host.install_packages(&to_install(name, version.as_ref()))?;
            }
        } else if install_type == "aio" {
            install_puppet_aio(
                host,
                &aio_install_url,
                version.as_deref(),
                collection.as_deref(),
                cleanup,
            )?;
        } else {
            log.warning(
                SOURCE,
                &format!("Unknown puppet install type '{install_type}'"),
            );
            run = false;
        }
    }

    // All three run whatever the config says: Python builds the default
    // before `get_cfg_option_str` can decide it is not needed (bug B96).
    let default_conf = value_or_error(get_config_value(host, puppet_bin, "config"))?;
    let default_ssl = value_or_error(get_config_value(host, puppet_bin, "ssldir"))?;
    let default_csr =
        value_or_error(get_config_value(host, puppet_bin, "csr_attributes"))?;
    let conf_file = cfg_str(&puppet_cfg, "conf_file").unwrap_or(default_conf);
    let ssl_dir = cfg_str(&puppet_cfg, "ssl_dir").unwrap_or(default_ssl);
    let csr_attributes_path =
        cfg_str(&puppet_cfg, "csr_attributes_path").unwrap_or(default_csr);

    let constants = Constants::new(&conf_file, &ssl_dir, &csr_attributes_path);

    if let Some(Value::Object(conf)) = puppet_cfg.get("conf") {
        let contents = host.load_text_file(&constants.conf_path)?;
        // The "cleaning" step upstream is unsure about, which is also what
        // breaks a file with continuation lines (bug B94).
        let cleaned: Vec<&str> = ci_core::pystr::split_lines(&contents)
            .into_iter()
            .map(str::trim_start)
            .collect();
        let mut puppet_config = RawConfigParser::new();
        puppet_config
            .read_str(&cleaned.join("\n"), &constants.conf_path)
            .map_err(|error| error.to_string())?;

        for (cfg_name, section) in conf {
            if cfg_name == "ca_cert" {
                host.ensure_dir(&constants.ssl_dir, Some(0o771))?;
                host.chownbyname(&constants.ssl_dir, puppet_user, "root")?;
                host.ensure_dir(&constants.ssl_cert_dir, None)?;
                host.chownbyname(&constants.ssl_cert_dir, puppet_user, "root")?;
                host.write_file(&constants.ssl_cert_path, &py_str(section), None)?;
                host.chownbyname(&constants.ssl_cert_path, puppet_user, "root")?;
            } else {
                let Value::Object(options) = section else {
                    return Err(format!(
                        "'{}' object has no attribute 'items'",
                        ci_config::type_name(section)
                    ));
                };
                for (option, value) in options {
                    let mut text = py_str(value);
                    if option == "certname" {
                        let fqdn = host.getfqdn();
                        text = text.replace("%f", &fqdn);
                        let iid = host.instance_id();
                        text = text.replace("%i", &iid);
                        text = text.to_lowercase();
                    }
                    puppet_config
                        .set(cfg_name, option, &text)
                        .map_err(|error| error.to_string())?;
                }
            }
            // Inside the loop, exactly as upstream has it (bug B93).
            host.rename(
                &constants.conf_path,
                &format!("{}.old", constants.conf_path),
            )?;
            host.write_file(&constants.conf_path, &puppet_config.stringify(), None)?;
        }
    }

    if let Some(csr) = puppet_cfg.get("csr_attributes") {
        host.write_file(
            &constants.csr_attributes_path,
            &ci_core::yamlfmt::dumps_plain(csr),
            None,
        )?;
    }

    if start_puppetd {
        manage_puppet_services(host, "enable", log);
    }

    if run {
        log.debug(SOURCE, "Running puppet-agent");
        let mut cmd = vec![puppet_bin.to_owned(), "agent".to_owned()];
        match puppet_cfg.get("exec_args") {
            Some(Value::Array(items)) => cmd.extend(items.iter().map(py_str)),
            Some(Value::String(text)) => {
                cmd.extend(text.split_whitespace().map(ToOwned::to_owned));
            }
            Some(other) => {
                log.warning(
                    SOURCE,
                    &format!(
                        "Unknown type {} provided for puppet 'exec_args' expected \
                         list, tuple, or string",
                        py_type_repr(other)
                    ),
                );
                cmd.extend(AGENT_DEFAULT_ARGS.iter().map(|arg| (*arg).to_owned()));
            }
            None => cmd.extend(AGENT_DEFAULT_ARGS.iter().map(|arg| (*arg).to_owned())),
        }
        host.subp(&cmd, false).map_err(|error| error.to_string())?;
    }

    if start_puppetd {
        manage_puppet_services(host, "start", log);
    }
    Ok(())
}

/// `[[name, version]]` or `[name]`, which is how `install_packages` is asked
/// for a pinned or an unpinned package.
fn to_install(name: &str, version: Option<&String>) -> Value {
    match version.filter(|v| !v.is_empty()) {
        Some(version) => Value::Array(vec![Value::Array(vec![
            Value::String(name.to_owned()),
            Value::String(version.clone()),
        ])]),
        None => Value::Array(vec![Value::String(name.to_owned())]),
    }
}

/// `util.get_cfg_option_str` without a default: present means `str(value)`,
/// so a `null` in the config becomes the four characters `None`.
fn cfg_str(cfg: &Object, key: &str) -> Option<String> {
    cfg.get(key).map(py_str)
}

fn value_or_error(result: Result<String, CmdError>) -> Result<String, String> {
    result.map_err(|error| error.to_string())
}

/// `type(x)` as `%s` prints it, for the `exec_args` warning.
fn py_type_repr(value: &Value) -> String {
    format!("<class '{}'>", ci_config::type_name(value))
}

/// The registry entry point.
///
/// # Errors
/// The module failure the stage reports.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let mut host = Live {
        root: args.root.to_owned(),
        distro: *args.distro,
        system_info: args.system_info.clone(),
        instance_id: args
            .datasource
            .as_ref()
            .map_or_else(String::new, |ds| ds.instance_id.to_owned()),
    };
    let (name, cfg) = (args.name.to_owned(), args.cfg.clone());
    handle_with(&name, &cfg, &mut host, &mut *args.logger)
}

/// The real machine.
#[derive(Debug, Clone)]
pub struct Live {
    root: std::path::PathBuf,
    distro: ci_distro::Distro,
    system_info: Object,
    instance_id: String,
}

impl Live {
    fn under_root(&self, path: &str) -> std::path::PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

impl Host for Live {
    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        super::rsyslog::install_packages(
            &self.root,
            &self.distro,
            &self.system_info,
            packages,
        )
    }

    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError> {
        let argv = ci_distro::service::command(&self.distro, action, service, &[])
            .map_err(|key| CmdError {
                command: key,
                ..CmdError::default()
            })?;
        self.subp(&argv, true).map(|_| ())
    }

    fn subp(&mut self, argv: &[String], capture: bool) -> Result<String, CmdError> {
        let command = ci_config::repr(&Value::Array(
            argv.iter().map(|arg| Value::String(arg.clone())).collect(),
        ));
        if !capture {
            let status = ci_sys::subp::Subp::new(argv)
                .inherit_env()
                .passthrough()
                .map_err(|error| CmdError {
                    command: command.clone(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: error.to_string(),
                })?;
            if status.success() {
                return Ok(String::new());
            }
            return Err(CmdError {
                command,
                exit_code: status.code(),
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let out = ci_sys::subp::Subp::new(argv)
            .run()
            .map_err(|error| CmdError {
                command: command.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: error.to_string(),
            })?;
        if out.code == Some(0) {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        Err(CmdError {
            command,
            exit_code: out.code,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn readurl(&mut self, url: &str) -> Result<String, String> {
        let response = ci_url::readurl(
            url,
            &ci_url::Config {
                retries: 5,
                ..ci_url::Config::default()
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(String::from_utf8_lossy(&response.contents).into_owned())
    }

    fn tempdir(&mut self) -> Result<String, String> {
        let base = self.under_root("var/tmp/cloud-init");
        std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        let dir = base.join(format!("puppet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(dir.to_string_lossy().into_owned())
    }

    fn load_text_file(&mut self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(self.under_root(path)).map_err(|error| {
            format!(
                "[Errno {}] {error}: '{path}'",
                error.raw_os_error().unwrap_or(0)
            )
        })
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        mode: Option<u32>,
    ) -> Result<(), String> {
        let target = self.under_root(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        ci_sys::atomic::write_file(
            &target,
            content.as_bytes(),
            ci_sys::atomic::WriteOptions {
                mode: mode.unwrap_or(0o644),
                ..ci_sys::atomic::WriteOptions::default()
            },
        )
        .map_err(|error| error.to_string())
    }

    fn ensure_dir(&mut self, path: &str, mode: Option<u32>) -> Result<(), String> {
        let target = self.under_root(path);
        std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn chownbyname(
        &mut self,
        path: &str,
        user: &str,
        group: &str,
    ) -> Result<(), String> {
        ci_sys::ids::chown_by_name(
            &self.root,
            &self.under_root(path),
            Some(user),
            Some(group),
        )
        .map_err(|error| error.to_string())
    }

    fn rename(&mut self, src: &str, dst: &str) -> Result<(), String> {
        std::fs::rename(self.under_root(src), self.under_root(dst))
            .map_err(|error| error.to_string())
    }

    fn getfqdn(&mut self) -> String {
        ci_core::hostname::getfqdn(&self.root)
    }

    fn instance_id(&mut self) -> String {
        self.instance_id.clone()
    }
}

/// A [`Host`] whose every answer is set up front, for the differential.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// Failures, keyed by the call as it is recorded.
    pub failures: Vec<(String, String)>,
    /// What `subp` prints, keyed by the call as it is recorded.
    pub stdout: Vec<(String, String)>,
    /// Files that already exist, by path.
    pub files: Vec<(String, String)>,
    /// `socket.getfqdn()`.
    pub fqdn: String,
    /// `cloud.get_instance_id()`.
    pub iid: String,
    /// The directory `temp_utils.tempdir` yields.
    pub tmpdir: String,
    pub calls: Vec<String>,
    /// What each file ended up holding, in the order they were first written.
    pub written: Vec<(String, String)>,
}

impl Fixture {
    fn record(&mut self, call: String) {
        self.calls.push(call);
    }

    fn failure(&self, call: &str) -> Option<String> {
        self.failures
            .iter()
            .find(|(key, _)| key == call)
            .map(|(_, error)| error.clone())
    }

    fn cmd_failure(&self, call: &str) -> Option<CmdError> {
        self.failure(call).map(|message| CmdError {
            command: call.to_owned(),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: message,
        })
    }
}

impl Host for Fixture {
    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        let call = format!("install_packages {}", ci_config::repr(packages));
        self.record(call.clone());
        self.failure(&call).map_or(Ok(()), Err)
    }

    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError> {
        let call = format!("manage_service {action} {service}");
        self.record(call.clone());
        self.cmd_failure(&call).map_or(Ok(()), Err)
    }

    fn subp(&mut self, argv: &[String], capture: bool) -> Result<String, CmdError> {
        let call = format!(
            "subp {} capture={}",
            argv.join(" "),
            if capture { "True" } else { "False" }
        );
        self.record(call.clone());
        if let Some(error) = self.cmd_failure(&call) {
            return Err(error);
        }
        Ok(self
            .stdout
            .iter()
            .find(|(key, _)| key == &call)
            .map_or_else(String::new, |(_, out)| out.clone()))
    }

    fn readurl(&mut self, url: &str) -> Result<String, String> {
        let call = format!("readurl {url}");
        self.record(call.clone());
        if let Some(error) = self.failure(&call) {
            return Err(error);
        }
        Ok(self
            .stdout
            .iter()
            .find(|(key, _)| key == &call)
            .map_or_else(String::new, |(_, out)| out.clone()))
    }

    fn tempdir(&mut self) -> Result<String, String> {
        self.record("tempdir".to_owned());
        self.failure("tempdir")
            .map_or_else(|| Ok(self.tmpdir.clone()), Err)
    }

    fn load_text_file(&mut self, path: &str) -> Result<String, String> {
        let call = format!("load_text_file {path}");
        self.record(call.clone());
        if let Some(error) = self.failure(&call) {
            return Err(error);
        }
        self.files
            .iter()
            .find(|(key, _)| key == path)
            .map(|(_, text)| text.clone())
            .ok_or_else(|| format!("[Errno 2] No such file or directory: '{path}'"))
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &str,
        mode: Option<u32>,
    ) -> Result<(), String> {
        let call = match mode {
            Some(mode) => format!("write_file {path} mode={mode:04o}"),
            None => format!("write_file {path}"),
        };
        self.record(call.clone());
        if let Some(error) = self.failure(&call) {
            return Err(error);
        }
        if let Some(slot) = self.files.iter_mut().find(|(key, _)| key == path) {
            content.clone_into(&mut slot.1);
        } else {
            self.files.push((path.to_owned(), content.to_owned()));
        }
        if let Some(slot) = self.written.iter_mut().find(|(key, _)| key == path) {
            content.clone_into(&mut slot.1);
        } else {
            self.written.push((path.to_owned(), content.to_owned()));
        }
        Ok(())
    }

    fn ensure_dir(&mut self, path: &str, mode: Option<u32>) -> Result<(), String> {
        let call = match mode {
            Some(mode) => format!("ensure_dir {path} mode={mode:04o}"),
            None => format!("ensure_dir {path}"),
        };
        self.record(call.clone());
        self.failure(&call).map_or(Ok(()), Err)
    }

    fn chownbyname(
        &mut self,
        path: &str,
        user: &str,
        group: &str,
    ) -> Result<(), String> {
        let call = format!("chownbyname {path} {user} {group}");
        self.record(call.clone());
        self.failure(&call).map_or(Ok(()), Err)
    }

    fn rename(&mut self, src: &str, dst: &str) -> Result<(), String> {
        let call = format!("rename {src} {dst}");
        self.record(call.clone());
        if let Some(error) = self.failure(&call) {
            return Err(error);
        }
        let existing = self
            .files
            .iter()
            .find(|(key, _)| key == src)
            .map(|(_, text)| text.clone());
        if let Some(text) = existing {
            if let Some(slot) = self.files.iter_mut().find(|(key, _)| key == dst) {
                text.clone_into(&mut slot.1);
            } else {
                self.files.push((dst.to_owned(), text.clone()));
            }
            if let Some(slot) = self.written.iter_mut().find(|(key, _)| key == dst) {
                text.clone_into(&mut slot.1);
            } else {
                self.written.push((dst.to_owned(), text));
            }
        }
        Ok(())
    }

    fn getfqdn(&mut self) -> String {
        self.record("getfqdn".to_owned());
        self.fqdn.clone()
    }

    fn instance_id(&mut self) -> String {
        self.record("get_instance_id".to_owned());
        self.iid.clone()
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
    use serde_json::json;

    fn object(value: serde_json::Value) -> Object {
        match value {
            serde_json::Value::Object(map) => map,
            _ => unreachable!("test config is an object"),
        }
    }

    fn fixture() -> Fixture {
        Fixture {
            stdout: vec![
                (
                    "subp puppet config print config capture=True".to_owned(),
                    "/etc/puppet/puppet.conf\n".to_owned(),
                ),
                (
                    "subp puppet config print ssldir capture=True".to_owned(),
                    "/var/lib/puppet/ssl\n".to_owned(),
                ),
                (
                    "subp puppet config print csr_attributes capture=True".to_owned(),
                    "/etc/puppet/csr_attributes.yaml\n".to_owned(),
                ),
            ],
            files: vec![(
                "/etc/puppet/puppet.conf".to_owned(),
                "[main]\norig = yes\n".to_owned(),
            )],
            fqdn: "host.example.com".to_owned(),
            iid: "i-abc".to_owned(),
            tmpdir: "/var/tmp/t".to_owned(),
            ..Fixture::default()
        }
    }

    fn run(cfg: serde_json::Value) -> (Fixture, Vec<String>, Result<(), String>) {
        let mut host = fixture();
        let mut log = Logger::capturing();
        let outcome = handle_with("puppet", &object(cfg), &mut host, &mut log);
        let lines = log.captured().to_vec();
        (host, lines, outcome)
    }

    #[test]
    fn no_puppet_key_is_a_debug_line_and_nothing_else() {
        let (host, lines, outcome) = run(json!({}));
        assert!(outcome.is_ok());
        assert!(host.calls.is_empty());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("no 'puppet' configuration found"));
    }

    #[test]
    fn the_package_names_are_tried_in_order_until_one_installs() {
        let mut host = fixture();
        host.failures.push((
            "install_packages ['puppet-agent']".to_owned(),
            "nope".to_owned(),
        ));
        let mut log = Logger::capturing();
        handle_with(
            "puppet",
            &object(json!({"puppet": {}})),
            &mut host,
            &mut log,
        )
        .unwrap();
        assert_eq!(host.calls[0], "install_packages ['puppet-agent']");
        assert_eq!(host.calls[1], "install_packages ['puppet']");
    }

    #[test]
    fn every_package_failing_is_a_warning_rather_than_an_error() {
        let mut host = fixture();
        for name in PACKAGE_NAMES {
            host.failures
                .push((format!("install_packages ['{name}']"), "nope".to_owned()));
        }
        let mut log = Logger::capturing();
        handle_with(
            "puppet",
            &object(json!({"puppet": {}})),
            &mut host,
            &mut log,
        )
        .unwrap();
        assert!(log
            .captured()
            .iter()
            .any(|line| line.contains("No installable puppet package in any of")));
    }

    #[test]
    fn puppet_config_print_runs_even_when_every_path_is_configured() {
        let (host, _, outcome) = run(json!({"puppet": {
            "install": false,
            "conf_file": "/c", "ssl_dir": "/s", "csr_attributes_path": "/a",
        }}));
        assert!(outcome.is_ok());
        assert_eq!(
            host.calls
                .iter()
                .filter(|call| call.contains("config print"))
                .count(),
            3
        );
    }

    #[test]
    fn the_backup_is_taken_once_per_section_not_once_per_run() {
        let (host, _, outcome) = run(json!({"puppet": {
            "install": false,
            "conf": {"main": {"a": "1"}, "agent": {"b": "2"}},
        }}));
        assert!(outcome.is_ok());
        assert_eq!(
            host.calls
                .iter()
                .filter(|c| c.starts_with("rename "))
                .count(),
            2
        );
        // The second rename copies the file the first write produced.
        let old = &host
            .written
            .iter()
            .find(|(path, _)| {
                std::path::Path::new(path)
                    .extension()
                    .is_some_and(|ext| ext == "old")
            })
            .unwrap()
            .1;
        assert_eq!(old, "[main]\norig = yes\na = 1\n\n");
    }

    #[test]
    fn certname_expands_the_fqdn_and_instance_id_and_is_downcased() {
        let (host, _, outcome) = run(json!({"puppet": {
            "install": false,
            "conf": {"agent": {"certname": "%f-%i-UPPER"}},
        }}));
        assert!(outcome.is_ok());
        let conf = &host
            .written
            .iter()
            .find(|(path, _)| path == "/etc/puppet/puppet.conf")
            .unwrap()
            .1;
        assert!(conf.contains("certname = host.example.com-i-abc-upper"));
    }

    #[test]
    fn a_lowercase_default_section_is_the_error_upstream_raises() {
        let (_, _, outcome) = run(json!({"puppet": {
            "install": false,
            "conf": {"default": {"a": "1"}},
        }}));
        assert_eq!(outcome.unwrap_err(), "No section: 'default'");
    }

    #[test]
    fn a_continuation_line_in_the_existing_file_fails_to_parse() {
        let mut host = fixture();
        host.files[0].1 = "[main]\nfoo = bar\n\tmore\n".to_owned();
        let mut log = Logger::capturing();
        let outcome = handle_with(
            "puppet",
            &object(
                json!({"puppet": {"install": false, "conf": {"main": {"a": "1"}}}}),
            ),
            &mut host,
            &mut log,
        );
        assert!(outcome.unwrap_err().contains("[line  3]: 'more'"));
    }

    #[test]
    fn exec_args_of_the_wrong_type_falls_back_to_the_default_args() {
        let (host, lines, outcome) = run(json!({"puppet": {
            "install": false, "exec": true, "exec_args": 5, "start_service": false,
        }}));
        assert!(outcome.is_ok());
        assert!(lines.iter().any(|line| line.contains("<class 'int'>")));
        assert!(host
            .calls
            .contains(&"subp puppet agent --test capture=False".to_owned()));
    }

    #[test]
    fn csr_attributes_are_dumped_at_pyyamls_own_indent_of_two() {
        let (host, _, outcome) = run(json!({"puppet": {
            "install": false, "start_service": false,
            "csr_attributes": {"custom_attributes": {"a": "b"}},
        }}));
        assert!(outcome.is_ok());
        assert_eq!(host.written[0].1, "custom_attributes:\n  a: b\n");
    }

    #[test]
    fn a_version_with_install_false_does_nothing_but_warn() {
        let (host, lines, outcome) = run(json!({"puppet": {
            "install": false, "version": "7", "start_service": false,
        }}));
        assert!(outcome.is_ok());
        assert!(lines.iter().any(|line| line.contains("doing nothing")));
        assert!(!host
            .calls
            .iter()
            .any(|call| call.starts_with("install_packages")));
    }
}
