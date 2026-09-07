//! Port of `cc_ansible.py`: install ansible, then run `ansible-pull`,
//! `ansible-galaxy` and `ansible-playbook` as configured.
//!
//! Everything the module does is a command, and which command depends on what
//! the previous one printed -- the ansible version decides whether the
//! playbooks are pulled together or one at a time, and the pip user base
//! decides what `PATH` the install runs under -- so the machine goes behind
//! [`Host`] and the differential compares the ordered call list, the log and
//! anything written to stdout.
//!
//! Three upstream behaviours are reproduced rather than corrected (bugs B97 to
//! B100 in `docs/COMPAT.md`): two messages that were meant to interpolate and
//! do not, a `PATH` entry built with the trailing newline of the command that
//! produced it, and the fact that `run_user` routes every command through
//! `distro.do_as`, which drops the environment the module spent that work
//! building.

use ci_config::{repr, type_name, Object, Value};
use ci_log::Logger;

use super::rsyslog::CmdError;
use super::Args;

const SOURCE: &str = "cc_ansible.py";

/// `CFG_OVERRIDE`, which is an environment variable name in lower case.
const CFG_OVERRIDE: &str = "ansible_config";

/// A `lifecycle.Version`, whose `-1` fields make a shorter version sort below
/// a longer one that shares its prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: i64,
    pub minor: i64,
    pub patch: i64,
    pub rev: i64,
}

impl Version {
    #[must_use]
    pub const fn new(major: i64, minor: i64, patch: i64) -> Self {
        Self {
            major,
            minor,
            patch,
            rev: -1,
        }
    }

    /// `Version.from_str`: up to four period-delimited integers.
    ///
    /// # Errors
    /// The `ValueError` a non-integer segment raises and the `TypeError` a
    /// fifth segment raises, both of which upstream lets escape.
    #[expect(
        clippy::should_implement_trait,
        reason = "upstream's constructor is Version.from_str, and it returns \
                  Python's error text rather than a parse error"
    )]
    pub fn from_str(text: &str) -> Result<Self, String> {
        let mut fields = [-1_i64; 4];
        let parts: Vec<&str> = text.split('.').collect();
        if parts.len() > 4 {
            return Err(format!(
                "Version.__new__() takes from 1 to 5 positional arguments but {} \
                 were given",
                parts.len() + 1
            ));
        }
        for (slot, part) in fields.iter_mut().zip(parts) {
            *slot = part.parse::<i64>().map_err(|_| {
                format!("invalid literal for int() with base 10: {}", repr_str(part))
            })?;
        }
        Ok(Self {
            major: fields[0],
            minor: fields[1],
            patch: fields[2],
            rev: fields[3],
        })
    }
}

fn repr_str(text: &str) -> String {
    ci_config::repr_str(text)
}

/// Which installer `install_method` selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Pip,
    Distro,
}

/// Everything `cc_ansible` asks of the machine.
pub trait Host {
    /// `subp.subp(command, update_env=env, cwd=cwd)`.
    fn subp(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
    ) -> Result<String, CmdError>;

    /// `distro.do_as(command, user, cwd=cwd)`, which builds a `su -` line and
    /// passes no environment (bug B99).
    fn do_as_user(
        &mut self,
        argv: &[String],
        user: &str,
        cwd: Option<&str>,
    ) -> Result<String, CmdError>;

    /// `subp.which(program)`.
    fn which(&mut self, program: &str) -> bool;

    /// `cloud.distro.install_packages(pkgs)`.
    fn install_packages(&mut self, packages: &Value) -> Result<(), String>;

    /// Whether `import pip` succeeds in the interpreter cloud-init runs under.
    fn has_pip(&mut self) -> bool;

    /// `os.path.exists(sysconfig.get_path("stdlib") + "/EXTERNALLY-MANAGED")`.
    fn externally_managed(&mut self) -> bool;

    /// `sys.stdout.write(text)`.
    fn stdout(&mut self, text: &str);
}

/// The `AnsiblePull` object: the environment it accumulates and the user it
/// runs as.
struct Pull<'a> {
    host: &'a mut dyn Host,
    method: Method,
    /// The interpreter upstream reaches through `sys.executable`.
    python: String,
    run_user: Option<String>,
    env: Vec<(String, String)>,
}

impl Pull<'_> {
    fn set_env(&mut self, key: &str, value: &str) {
        if let Some(slot) = self.env.iter_mut().find(|(name, _)| name == key) {
            value.clone_into(&mut slot.1);
        } else {
            self.env.push((key.to_owned(), value.to_owned()));
        }
    }

    /// `do_as`: through `su` when a `run_user` is set, directly otherwise.
    fn do_as(
        &mut self,
        argv: &[String],
        cwd: Option<&str>,
    ) -> Result<String, CmdError> {
        match self.run_user.clone() {
            Some(user) => self.host.do_as_user(argv, &user, cwd),
            None => self.host.subp(argv, &self.env, cwd),
        }
    }

    /// `is_installed`.
    fn is_installed(&mut self) -> Result<bool, CmdError> {
        match self.method {
            Method::Distro => Ok(self.host.which("ansible")),
            Method::Pip => {
                let mut cmd = vec![
                    self.python.clone(),
                    "-m".to_owned(),
                    "pip".to_owned(),
                    "list".to_owned(),
                ];
                if self.run_user.is_some() {
                    cmd.push("--user".to_owned());
                }
                Ok(self.do_as(&cmd, None)?.contains("ansible"))
            }
        }
    }

    /// `AnsiblePullPip.add_pip_install_site_to_path`.
    ///
    /// The user base arrives with the newline `pip` printed, and it is spliced
    /// into `PATH` unstripped (bug B100).
    fn add_pip_install_site_to_path(&mut self) -> Result<(), CmdError> {
        let Some(_) = self.run_user.clone() else {
            return Ok(());
        };
        let cmd = vec![
            self.python.clone(),
            "-c".to_owned(),
            "import site; print(site.getuserbase())".to_owned(),
        ];
        let user_base = self.do_as(&cmd, None)?;
        let ansible_path = format!("{user_base}/bin/");
        let old = self
            .env
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty());
        let path = match old {
            Some(old) => format!("{old}:{ansible_path}"),
            None => ansible_path,
        };
        self.set_env("PATH", &path);
        Ok(())
    }

    /// `install`.
    fn install(&mut self, package: &str, log: &mut Logger) -> Result<(), String> {
        match self.method {
            Method::Distro => {
                if !self.is_installed().map_err(|e| e.to_string())? {
                    let pkgs = Value::Array(vec![Value::String(package.to_owned())]);
                    self.host.install_packages(&pkgs)?;
                }
                Ok(())
            }
            Method::Pip => {
                if !self.host.has_pip() {
                    let pkgs =
                        Value::Array(vec![Value::String(PIP_PACKAGE.to_owned())]);
                    self.host.install_packages(&pkgs)?;
                }
                if self.is_installed().map_err(|e| e.to_string())? {
                    return Ok(());
                }
                let mut cmd = vec![
                    self.python.clone(),
                    "-m".to_owned(),
                    "pip".to_owned(),
                    "install".to_owned(),
                ];
                if self.host.externally_managed() {
                    cmd.push("--break-system-packages".to_owned());
                }
                if self.run_user.is_some() {
                    cmd.push("--user".to_owned());
                }
                // `__upgrade_pip`, whose failure is a warning and not an error.
                log.info(SOURCE, "Upgrading pip");
                let mut upgrade = cmd.clone();
                upgrade.push("--upgrade".to_owned());
                upgrade.push("pip".to_owned());
                match self.do_as(&upgrade, None) {
                    Ok(_) => log.info(SOURCE, "Upgraded pip"),
                    Err(error) => log.warning(
                        SOURCE,
                        &format!(
                            "Failed at upgrading pip. This is usually not \
                             criticalso the script will skip this step.\n{error}"
                        ),
                    ),
                }
                log.info(SOURCE, &format!("Installing the {package} package"));
                let mut install = cmd;
                install.push(package.to_owned());
                self.do_as(&install, None).map_err(|e| e.to_string())?;
                log.info(SOURCE, &format!("Installed the {package} package"));
                Ok(())
            }
        }
    }

    /// `get_version`.
    fn get_version(&mut self) -> Result<Option<Version>, String> {
        let argv = vec!["ansible-pull".to_owned(), "--version".to_owned()];
        let stdout = self.do_as(&argv, None).map_err(|e| e.to_string())?;
        let lines = ci_core::pystr::split_lines(&stdout);
        let Some(first) = lines.first() else {
            return Err("pop from empty list".to_owned());
        };
        let Some(found) = first.find(|c: char| c.is_ascii_digit() || c == '.') else {
            return Ok(None);
        };
        let rest = first.get(found..).unwrap_or("");
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        Version::from_str(rest.get(..end).unwrap_or("")).map(Some)
    }

    /// `pull`.
    fn pull(&mut self, args: &[String]) -> Result<String, CmdError> {
        let mut command = vec!["ansible-pull".to_owned()];
        command.extend_from_slice(args);
        self.do_as(&command, None)
    }
}

/// `distro.pip_package_name`, which is the same on every distro cloud-init
/// ships a class for.
const PIP_PACKAGE: &str = "python3-pip";

/// `handle`, against a scripted machine.
///
/// # Errors
/// Everything upstream lets escape: a config that fails `validate_config`, an
/// install that did not take, an unparsable version, a command that failed.
pub fn handle_with(
    cfg: &Object,
    python: &str,
    home: &str,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    let ansible_cfg = match cfg.get("ansible") {
        None => Object::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(other) => {
            return Err(format!(
                "'{}' object has no attribute 'get'",
                type_name(other)
            ))
        }
    };
    if ansible_cfg.is_empty() {
        return Ok(());
    }

    validate_config(&ansible_cfg)?;

    let install_method = ansible_cfg.get("install_method").and_then(Value::as_str);
    let method = if install_method == Some("pip") {
        Method::Pip
    } else {
        Method::Distro
    };
    let run_user = ansible_cfg
        .get("run_user")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let package_name = ansible_cfg
        .get("package_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();

    let mut ansible = Pull {
        host,
        method,
        python: python.to_owned(),
        run_user,
        env: vec![("HOME".to_owned(), home.to_owned())],
    };
    if method == Method::Pip {
        ansible
            .add_pip_install_site_to_path()
            .map_err(|error| error.to_string())?;
    }

    ansible.install(&package_name, log)?;
    // `check_deps`.
    if !ansible.is_installed().map_err(|e| e.to_string())? {
        return Err("command: ansible is not installed".to_owned());
    }

    if let Some(config) = ansible_cfg
        .get("ansible_config")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        let config = config.to_owned();
        ansible.set_env(CFG_OVERRIDE, &config);
    }

    if let Some(galaxy) = ansible_cfg.get("galaxy").filter(|v| truthy(v)) {
        ansible_galaxy(galaxy, &mut ansible, log)?;
    }

    if let Some(pull_cfg) = ansible_cfg.get("pull").filter(|v| truthy(v)) {
        for one in as_pull_list(pull_cfg) {
            run_ansible_pull(&mut ansible, &one, log)?;
        }
    }

    if let Some(controller) = ansible_cfg.get("setup_controller").filter(|v| truthy(v))
    {
        ansible_controller(controller, &mut ansible)?;
    }
    Ok(())
}

/// `pull` accepts one mapping or a list of them.
fn as_pull_list(value: &Value) -> Vec<Object> {
    match value {
        Value::Object(map) => vec![map.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_object().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

/// `validate_config`.
///
/// # Errors
/// The `ValueError` each missing or malformed key raises, message included.
pub fn validate_config(cfg: &Object) -> Result<(), String> {
    let shown = repr(&Value::Object(cfg.clone()));
    for key in ["install_method", "package_name"] {
        if !cfg.get(key).is_some_and(truthy) {
            return Err(format!("Missing required key '{key}' from {shown}"));
        }
    }

    if let Some(pull_cfg) = cfg.get("pull").filter(|v| truthy(v)) {
        let items: Vec<Value> = match pull_cfg {
            Value::Object(map) => vec![Value::Object(map.clone())],
            Value::Array(items) => items.clone(),
            other => {
                return Err(format!(
                    "Invalid value ansible.pull. Expected either dict of list of \
                     dicts but found {}",
                    py_str_of(other)
                ))
            }
        };
        for item in &items {
            let Value::Object(p_cfg) = item else {
                return Err(format!(
                    "Invalid value of ansible.pull. Expected dict but found {}",
                    py_str_of(item)
                ));
            };
            let shown = repr(item);
            if !p_cfg.get("url").is_some_and(truthy) {
                return Err(format!("Missing required key 'url' from {shown}"));
            }
            let has_playbook = p_cfg.get("playbook_name").is_some_and(truthy);
            let has_playbooks = p_cfg.get("playbook_names").is_some_and(truthy);
            if !has_playbook && !has_playbooks {
                return Err(format!(
                    "Missing required key 'playbook_names' from {shown}"
                ));
            }
            if has_playbook && has_playbooks {
                return Err(format!(
                    "Key 'ansible.pull.playbook_name' and \
                     'ansible.pull.playbook_names' are mutually exclusive. Please \
                     use 'playbook_names' in {shown}"
                ));
            }
        }
    }

    if let Some(controller) = cfg.get("setup_controller").filter(|v| truthy(v)) {
        let Value::Object(map) = controller else {
            return Err(format!(
                "'{}' object has no attribute 'get'",
                type_name(controller)
            ));
        };
        if !map.get("repositories").is_some_and(truthy)
            && !map.get("run_ansible").is_some_and(truthy)
        {
            return Err(format!("Missing required key from {}", repr(controller)));
        }
    }

    // Reached only for a method that is neither, since the required-key check
    // above already rejected a missing one. The message was meant to name the
    // method and does not (bug B97).
    let install = cfg
        .get("install_method")
        .map_or("", |v| v.as_str().unwrap_or(""));
    if install != "pip" && install != "distro" {
        return Err("Invalid install method {install}".to_owned());
    }
    Ok(())
}

/// `filter_args`: underscores become dashes and a `False` is dropped, while a
/// `None`, a `0` and an empty string all survive to the command line.
fn filter_args(cfg: &Object) -> Vec<(String, Value)> {
    cfg.iter()
        .filter(|(_, value)| !matches!(value, Value::Bool(false)))
        .map(|(key, value)| (key.replace('_', "-"), value.clone()))
        .collect()
}

/// `run_ansible_pull`.
fn run_ansible_pull(
    ansible: &mut Pull<'_>,
    cfg: &Object,
    log: &mut Logger,
) -> Result<(), String> {
    let mut cfg = cfg.clone();
    let playbook_name = cfg.shift_remove("playbook_name");
    let playbook_names = cfg.shift_remove("playbook_names");
    let playbooks: Vec<String> = match playbook_name.filter(truthy) {
        Some(one) => vec![py_str_of(&one)],
        None => match playbook_names {
            Some(Value::Array(items)) => items.iter().map(py_str_of).collect(),
            Some(other) => return Err(iteration_error(&other)),
            None => return Err(iteration_error(&Value::Null)),
        },
    };

    let version = ansible.get_version()?;
    match version {
        None => log.warning(SOURCE, "Cannot parse ansible version"),
        Some(version) if version < Version::new(2, 7, 0) => {
            if cfg.get("diff").is_some_and(truthy) {
                // The message is missing the space before "doesn't" (bug B98).
                return Err(format!(
                    "Ansible version {}.{}.{}doesn't support --diff flag, exiting.",
                    version.major, version.minor, version.patch
                ));
            }
        }
        Some(_) => {}
    }

    let pull_args: Vec<String> = filter_args(&cfg)
        .into_iter()
        .map(|(key, value)| match value {
            Value::Bool(true) => format!("--{key}"),
            other => format!("--{key}={}", py_str_of(&other)),
        })
        .collect();

    if version.is_some_and(|version| version >= Version::new(2, 12, 0)) {
        let mut args = pull_args;
        args.extend(playbooks);
        let stdout = ansible.pull(&args).map_err(|e| e.to_string())?;
        if !stdout.is_empty() {
            ansible.host.stdout(&stdout);
        }
        return Ok(());
    }
    for playbook in playbooks {
        let mut args = pull_args.clone();
        args.push(playbook);
        let stdout = ansible.pull(&args).map_err(|e| e.to_string())?;
        if !stdout.is_empty() {
            ansible.host.stdout(&stdout);
        }
    }
    Ok(())
}

/// `ansible_galaxy`.
fn ansible_galaxy(
    cfg: &Value,
    ansible: &mut Pull<'_>,
    log: &mut Logger,
) -> Result<(), String> {
    let Value::Object(map) = cfg else {
        return Err(format!(
            "'{}' object has no attribute 'get'",
            type_name(cfg)
        ));
    };
    let actions = match map.get("actions") {
        Some(Value::Array(items)) => items.clone(),
        Some(other) => return Err(iteration_error(other)),
        None => Vec::new(),
    };
    if actions.is_empty() {
        log.warning(SOURCE, &format!("Invalid config: {}", repr(cfg)));
    }
    for command in actions {
        let Value::Array(argv) = command else {
            return Err(iteration_error(&command));
        };
        let argv: Vec<String> = argv.iter().map(py_str_of).collect();
        ansible.do_as(&argv, None).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `ansible_controller`.
fn ansible_controller(cfg: &Value, ansible: &mut Pull<'_>) -> Result<(), String> {
    let Value::Object(map) = cfg else {
        return Err(format!(
            "'{}' object has no attribute 'get'",
            type_name(cfg)
        ));
    };
    if let Some(Value::Array(repositories)) = map.get("repositories") {
        for repository in repositories {
            let Value::Object(entry) = repository else {
                return Err(subscript_error(repository));
            };
            let (Some(source), Some(path)) = (entry.get("source"), entry.get("path"))
            else {
                return Err("'source'".to_owned());
            };
            let argv = vec![
                "git".to_owned(),
                "clone".to_owned(),
                py_str_of(source),
                py_str_of(path),
            ];
            ansible.do_as(&argv, None).map_err(|e| e.to_string())?;
        }
    }
    if let Some(Value::Array(runs)) = map.get("run_ansible") {
        for args in runs {
            let Value::Object(entry) = args else {
                return Err(subscript_error(args));
            };
            let mut entry = entry.clone();
            let Some(playbook_dir) = entry.shift_remove("playbook_dir") else {
                return Err("'playbook_dir'".to_owned());
            };
            let Some(playbook_name) = entry.shift_remove("playbook_name") else {
                return Err("'playbook_name'".to_owned());
            };
            let mut argv =
                vec!["ansible-playbook".to_owned(), py_str_of(&playbook_name)];
            argv.extend(
                filter_args(&entry)
                    .into_iter()
                    .map(|(key, value)| format!("--{key}={}", py_str_of(&value))),
            );
            ansible
                .do_as(&argv, Some(&py_str_of(&playbook_dir)))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn iteration_error(value: &Value) -> String {
    format!("'{}' object is not iterable", type_name(value))
}

fn subscript_error(value: &Value) -> String {
    format!("{} indices must be integers or slices", type_name(value))
}

/// Plain `bool(value)`.
fn truthy(value: &Value) -> bool {
    ci_config::option::py_truthy(value)
}

/// `str(value)` in an f-string.
fn py_str_of(value: &Value) -> String {
    super::py_str(value)
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
    };
    let cfg = args.cfg.clone();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
    handle_with(&cfg, PYTHON, &home, &mut host, &mut *args.logger)
}

/// The interpreter upstream finds in `sys.executable`.
///
/// The port is not a Python program and has none, so it names the one every
/// distro cloud-init supports ships (deviation 165).
const PYTHON: &str = "/usr/bin/python3";

/// `shlex.quote`, which `distro.do_as` uses to build the `su -c` line.
fn shlex_quote(arg: &str) -> String {
    const SAFE: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ\
                        0123456789_@%+=:,./-";
    if arg.is_empty() {
        return "''".to_owned();
    }
    if arg.chars().all(|c| SAFE.contains(c)) {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

/// The real machine.
#[derive(Debug, Clone)]
pub struct Live {
    root: std::path::PathBuf,
    distro: ci_distro::Distro,
    system_info: Object,
}

impl Host for Live {
    fn subp(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
    ) -> Result<String, CmdError> {
        let command = repr(&Value::Array(
            argv.iter().map(|arg| Value::String(arg.clone())).collect(),
        ));
        let mut builder = ci_sys::subp::Subp::new(argv).inherit_env();
        for (key, value) in env {
            builder = builder.env(key, value);
        }
        if let Some(cwd) = cwd {
            builder = builder.cwd(cwd);
        }
        let out = builder.run().map_err(|error| CmdError {
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

    fn do_as_user(
        &mut self,
        argv: &[String],
        user: &str,
        cwd: Option<&str>,
    ) -> Result<String, CmdError> {
        let directory = cwd.map_or_else(String::new, |dir| format!("cd {dir} && "));
        let quoted: Vec<String> = argv.iter().map(|arg| shlex_quote(arg)).collect();
        let line = format!("{directory}env PATH=$PATH {}", quoted.join(" "));
        let su = vec![
            "su".to_owned(),
            "-".to_owned(),
            user.to_owned(),
            "-c".to_owned(),
            line,
        ];
        self.subp(&su, &[], None)
    }

    fn which(&mut self, program: &str) -> bool {
        ci_sys::subp::which(program).is_some()
    }

    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        super::rsyslog::install_packages(
            &self.root,
            &self.distro,
            &self.system_info,
            packages,
        )
    }

    fn has_pip(&mut self) -> bool {
        ci_sys::subp::Subp::new([PYTHON, "-c", "import pip"])
            .run()
            .is_ok_and(|out| out.code == Some(0))
    }

    fn externally_managed(&mut self) -> bool {
        // `sysconfig.get_path("stdlib")` on every distro cloud-init supports.
        std::fs::read_dir(self.root.join("usr/lib"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("python3"))
            .any(|entry| entry.path().join("EXTERNALLY-MANAGED").exists())
    }

    fn stdout(&mut self, text: &str) {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
}

/// A [`Host`] whose every answer is set up front, for the differential.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// Failures, keyed by the call as it is recorded.
    pub failures: Vec<(String, String)>,
    /// What a command prints, keyed by the call as it is recorded.
    pub stdout: Vec<(String, String)>,
    /// Programs `which` finds.
    pub present: Vec<String>,
    /// Whether `import pip` succeeds.
    pub pip: bool,
    /// Whether the stdlib is marked externally managed.
    pub managed: bool,
    pub calls: Vec<String>,
    /// What the module wrote to stdout.
    pub console: Vec<String>,
}

impl Fixture {
    fn record(&mut self, call: String) {
        self.calls.push(call);
    }

    fn answer(&self, call: &str) -> Result<String, CmdError> {
        if let Some((_, message)) = self.failures.iter().find(|(key, _)| key == call) {
            return Err(CmdError {
                command: call.to_owned(),
                exit_code: Some(1),
                stdout: String::new(),
                stderr: message.clone(),
            });
        }
        Ok(self
            .stdout
            .iter()
            .find(|(key, _)| key == call)
            .map_or_else(String::new, |(_, out)| out.clone()))
    }
}

impl Host for Fixture {
    fn subp(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
    ) -> Result<String, CmdError> {
        let env: Vec<String> = env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let call = format!(
            "subp {} env=[{}] cwd={}",
            argv.join(" "),
            env.join(","),
            cwd.unwrap_or("")
        );
        self.record(call.clone());
        self.answer(&call)
    }

    fn do_as_user(
        &mut self,
        argv: &[String],
        user: &str,
        cwd: Option<&str>,
    ) -> Result<String, CmdError> {
        let call = format!("do_as {user} {} cwd={}", argv.join(" "), cwd.unwrap_or(""));
        self.record(call.clone());
        self.answer(&call)
    }

    fn which(&mut self, program: &str) -> bool {
        self.record(format!("which {program}"));
        self.present.iter().any(|name| name == program)
    }

    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        let call = format!("install_packages {}", repr(packages));
        self.record(call.clone());
        self.failures
            .iter()
            .find(|(key, _)| key == &call)
            .map_or(Ok(()), |(_, message)| Err(message.clone()))
    }

    fn has_pip(&mut self) -> bool {
        self.record("import pip".to_owned());
        self.pip
    }

    fn externally_managed(&mut self) -> bool {
        self.record("externally_managed".to_owned());
        self.managed
    }

    fn stdout(&mut self, text: &str) {
        self.console.push(text.to_owned());
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

    fn run(
        cfg: serde_json::Value,
        host: &mut Fixture,
    ) -> (Vec<String>, Result<(), String>) {
        let mut log = Logger::capturing();
        let outcome =
            handle_with(&object(cfg), "/usr/bin/python3", "/root", host, &mut log);
        (log.captured().to_vec(), outcome)
    }

    fn distro_host() -> Fixture {
        Fixture {
            present: vec!["ansible".to_owned()],
            stdout: vec![(
                "subp ansible-pull --version env=[HOME=/root] cwd=".to_owned(),
                "ansible-pull [core 2.17.6]\n".to_owned(),
            )],
            ..Fixture::default()
        }
    }

    #[test]
    fn no_ansible_key_does_nothing_at_all() {
        let mut host = Fixture::default();
        let (lines, outcome) = run(json!({}), &mut host);
        assert!(outcome.is_ok());
        assert!(host.calls.is_empty());
        assert!(lines.is_empty());
    }

    #[test]
    fn the_required_keys_are_named_in_the_error() {
        let mut host = Fixture::default();
        let (_, outcome) =
            run(json!({"ansible": {"package_name": "ansible"}}), &mut host);
        assert_eq!(
            outcome.unwrap_err(),
            "Missing required key 'install_method' from {'package_name': 'ansible'}"
        );
    }

    #[test]
    fn a_bad_install_method_gets_the_message_that_forgot_its_f_prefix() {
        let mut host = Fixture::default();
        let (_, outcome) = run(
            json!({"ansible": {"install_method": "apt", "package_name": "ansible"}}),
            &mut host,
        );
        assert_eq!(outcome.unwrap_err(), "Invalid install method {install}");
    }

    #[test]
    fn an_already_present_ansible_is_not_installed_again() {
        let mut host = distro_host();
        let (_, outcome) = run(
            json!({"ansible": {"install_method": "distro", "package_name": "ansible"}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(!host.calls.iter().any(|c| c.starts_with("install_packages")));
    }

    #[test]
    fn a_missing_ansible_after_install_is_the_check_deps_error() {
        let mut host = Fixture::default();
        let (_, outcome) = run(
            json!({"ansible": {"install_method": "distro", "package_name": "ansible"}}),
            &mut host,
        );
        assert_eq!(outcome.unwrap_err(), "command: ansible is not installed");
    }

    #[test]
    fn a_modern_ansible_pulls_every_playbook_in_one_command() {
        let mut host = distro_host();
        let (_, outcome) = run(
            json!({"ansible": {
                "install_method": "distro", "package_name": "ansible",
                "pull": {"url": "u", "playbook_names": ["a.yml", "b.yml"]},
            }}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call.contains("ansible-pull --url=u a.yml b.yml")));
    }

    #[test]
    fn an_old_ansible_pulls_them_one_at_a_time() {
        let mut host = distro_host();
        host.stdout[0].1 = "ansible-pull 2.10.8\n".to_owned();
        let (_, outcome) = run(
            json!({"ansible": {
                "install_method": "distro", "package_name": "ansible",
                "pull": {"url": "u", "playbook_names": ["a.yml", "b.yml"]},
            }}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(host
            .calls
            .iter()
            .any(|call| call.contains("ansible-pull --url=u a.yml env=")));
        assert!(host
            .calls
            .iter()
            .any(|call| call.contains("ansible-pull --url=u b.yml env=")));
    }

    #[test]
    fn a_false_argument_is_dropped_and_a_true_one_loses_its_value() {
        let mut host = distro_host();
        let (_, outcome) = run(
            json!({"ansible": {
                "install_method": "distro", "package_name": "ansible",
                "pull": {"url": "u", "playbook_name": "a.yml",
                         "full": true, "clean": false, "accept_host_key": true},
            }}),
            &mut host,
        );
        assert!(outcome.is_ok());
        let call = host
            .calls
            .iter()
            .find(|call| call.contains("ansible-pull") && !call.contains("--version"))
            .unwrap();
        assert!(call.contains("--full"));
        assert!(call.contains("--accept-host-key"));
        assert!(!call.contains("clean"));
    }

    #[test]
    fn a_run_user_routes_through_su_and_loses_the_environment() {
        let mut host = Fixture {
            present: vec!["ansible".to_owned()],
            stdout: vec![(
                "do_as ann ansible-pull --version cwd=".to_owned(),
                "ansible-pull [core 2.17.6]\n".to_owned(),
            )],
            ..Fixture::default()
        };
        let (_, outcome) = run(
            json!({"ansible": {
                "install_method": "distro", "package_name": "ansible",
                "run_user": "ann", "ansible_config": "/etc/ansible.cfg",
                "pull": {"url": "u", "playbook_name": "a.yml"},
            }}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(host
            .calls
            .iter()
            .all(|call| !call.contains("ansible_config")));
    }

    #[test]
    fn an_unparsable_version_is_a_warning_and_the_pull_still_runs() {
        let mut host = distro_host();
        host.stdout[0].1 = "no version here\n".to_owned();
        let (lines, outcome) = run(
            json!({"ansible": {
                "install_method": "distro", "package_name": "ansible",
                "pull": {"url": "u", "playbook_name": "a.yml"},
            }}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(lines
            .iter()
            .any(|line| line.contains("Cannot parse ansible version")));
    }

    #[test]
    fn an_old_ansible_asked_for_a_diff_is_an_error_with_a_missing_space() {
        let mut host = distro_host();
        host.stdout[0].1 = "ansible-pull 2.6.0\n".to_owned();
        let (_, outcome) = run(
            json!({"ansible": {
                "install_method": "distro", "package_name": "ansible",
                "pull": {"url": "u", "playbook_name": "a.yml", "diff": true},
            }}),
            &mut host,
        );
        assert_eq!(
            outcome.unwrap_err(),
            "Ansible version 2.6.0doesn't support --diff flag, exiting."
        );
    }

    #[test]
    fn the_controller_loses_the_keys_it_pops() {
        let mut host = distro_host();
        let (_, outcome) = run(
            json!({"ansible": {
                "install_method": "distro", "package_name": "ansible",
                "setup_controller": {"run_ansible": [
                    {"playbook_dir": "/d", "playbook_name": "p.yml", "extra": 1}
                ]},
            }}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(host.calls.iter().any(|call| call
            == "subp ansible-playbook p.yml --extra=1 env=[HOME=/root] cwd=/d"));
    }

    #[test]
    fn versions_sort_with_the_more_specific_number_larger() {
        assert!(Version::from_str("3.9.9.9").unwrap() < Version::new(3, 10, -1));
        assert!(
            Version::from_str("2.9.1").unwrap() > Version::from_str("2.9").unwrap()
        );
        assert_eq!(
            Version::from_str("2.").unwrap_err(),
            "invalid literal for int() with base 10: ''"
        );
    }
}
