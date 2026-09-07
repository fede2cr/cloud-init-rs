//! `distros/package_management/` and the `Distro.install_packages` family.
//!
//! Upstream splits this three ways: a `PackageManager` subclass per tool
//! (`apt.py`, `snap.py`), `Distro.install_packages` dispatching a `packages:`
//! list across the managers a distro registers, and `Distro.package_command`
//! for the one caller that still wants a raw subcommand. All three end in
//! `subp.subp`.
//!
//! Nothing here runs anything. Every entry point returns the [`Step`]s that
//! *would* run, in order, so that the decision is comparable against Python
//! without an `apt-get install` on the host doing the comparing — the same
//! split [`crate::hostname`] uses. [`run`] executes a plan.
//!
//! # Ordering
//!
//! Upstream funnels the package list through `set`s, so the argv it builds is
//! in Python set order — which for strings is salted by `PYTHONHASHSEED` and
//! therefore differs on every boot (upstream bug B72). This port keeps
//! first-seen config order instead; see COMPAT.md deviation 130.

use std::collections::HashSet;
use std::time::Duration;

use ci_config::{Object, Value};
use ci_core::semaphore::Frequency;
use ci_log::Logger;

/// `APT_GET_COMMAND`.
pub const APT_GET_COMMAND: &[&str] = &[
    "apt-get",
    "--option=Dpkg::Options::=--force-confold",
    "--option=Dpkg::options::=--force-unsafe-io",
    "--assume-yes",
    "--quiet",
];

/// `APT_LOCK_FILES`, in the order upstream takes them.
pub const APT_LOCK_FILES: &[&str] = &[
    "/var/lib/dpkg/lock-frontend",
    "/var/lib/dpkg/lock",
    "/var/cache/apt/archives/lock",
    "/var/lib/apt/lists/lock",
];

/// `APT_LOCK_WAIT_TIMEOUT`.
pub const APT_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The default `apt_get_wrapper` command, used when the key is absent.
pub const DEFAULT_APT_WRAPPER: &[&str] = &["eatmydata"];

/// A package manager `packages:` can name, i.e. a key of
/// `known_package_managers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Manager {
    Apt,
    Snap,
}

impl Manager {
    /// The class's `name` attribute, which is also its `packages:` key.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Snap => "snap",
        }
    }

    /// `known_package_managers[name]`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "apt" => Some(Self::Apt),
            "snap" => Some(Self::Snap),
            _ => None,
        }
    }
}

/// One `subp.subp` the package layer would make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub argv: Vec<String>,
    /// `update_env`: added to the inherited environment rather than replacing
    /// it.
    pub env: Vec<(String, String)>,
    /// `capture`. False lets the child write to cloud-init's own stdout, which
    /// is how `apt-get`'s progress reaches the console.
    pub capture: bool,
    /// Whether the caller takes the apt locks first — `_wait_for_apt_command`.
    pub wait_for_apt_lock: bool,
    /// `helpers.Runners.run(name, .., freq)`, for the calls upstream
    /// semaphores. `update-sources` is the only one.
    pub semaphore: Option<(String, Frequency)>,
}

impl Step {
    fn plain(argv: Vec<String>) -> Self {
        Self {
            argv,
            env: Vec::new(),
            capture: true,
            wait_for_apt_lock: false,
            semaphore: None,
        }
    }
}

/// One validated element of a `packages:` list — `Distro._validate_entry`.
///
/// The pair keeps [`Value`]s rather than strings because upstream formats it
/// with `"%s=%s" %`, which stringifies whatever it is given: `[nginx, 1.2]`
/// is a valid entry and means `nginx=1.2`.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Plain(String),
    Pair(Value, Value),
}

impl Entry {
    /// `util.expand_package_list("%s=%s", [entry])`, for a single entry.
    #[must_use]
    pub fn expand(&self) -> String {
        match self {
            Self::Plain(name) => name.clone(),
            // `if len(pkg) == 2 and pkg[1]` — a falsy version drops the
            // format and leaves the bare name.
            Self::Pair(name, version) => {
                if truthy(version) {
                    format!("{}={}", py_str(name), py_str(version))
                } else {
                    py_str(name)
                }
            }
        }
    }
}

/// Python truthiness, for the `and pkg[1]` guard.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `str()` of a config scalar.
fn py_str(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| ci_config::repr(value), ToOwned::to_owned)
}

/// Python's `repr` of a list of strings.
fn repr_list(items: &[String]) -> String {
    let shown: Vec<String> = items.iter().map(|s| ci_config::repr_str(s)).collect();
    format!("[{}]", shown.join(", "))
}

/// Python's `repr` of a set of strings; an empty one prints as `set()`.
///
/// Upstream prints these in hash order, which differs per boot (B72). The
/// caller sorts so that the message is reproducible.
fn repr_set(items: &[String]) -> String {
    if items.is_empty() {
        return "set()".to_owned();
    }
    let shown: Vec<String> = items.iter().map(|s| ci_config::repr_str(s)).collect();
    format!("{{{}}}", shown.join(", "))
}

/// `Distro._validate_entry`.
///
/// The `Err` is the `ValueError` upstream raises, which escapes `handle` and
/// so aborts the module.
fn validate_entry(entry: &Value) -> Result<Entry, String> {
    if let Some(name) = entry.as_str() {
        return Ok(Entry::Plain(name.to_owned()));
    }
    if let Some(items) = entry.as_array() {
        if let [first, second] = items.as_slice() {
            return Ok(Entry::Pair(first.clone(), second.clone()));
        }
    }
    Err("Invalid 'packages' yaml specification. Check schema definition.".to_owned())
}

/// The result of `Distro._extract_package_by_manager`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Extracted {
    /// Packages named under an explicit manager key, in first-seen order.
    pub by_manager: Vec<(Manager, Vec<Entry>)>,
    /// Everything named bare, in first-seen order.
    pub generic: Vec<Entry>,
}

/// `Distro._extract_package_by_manager`.
///
/// Upstream builds `defaultdict(set)` + `set`; this keeps insertion order and
/// de-duplicates on the expanded string, which is what the sets compare on
/// once `expand_package_list` has run.
///
/// An unknown manager key is logged and its packages dropped, exactly as
/// upstream's `except KeyError` does — the boot is not failed over it.
///
/// # Errors
///
/// The `ValueError` from `_validate_entry`, for an entry that is neither a
/// string nor a two-element list.
pub fn extract(pkglist: &[Value], log: &mut Logger) -> Result<Extracted, String> {
    let mut by_manager: Vec<(Manager, Vec<Entry>)> = Vec::new();
    let mut generic: Vec<Entry> = Vec::new();
    let mut generic_seen: HashSet<String> = HashSet::new();

    for entry in pkglist {
        let Some(mapping) = entry.as_object() else {
            let validated = validate_entry(entry)?;
            if generic_seen.insert(validated.expand()) {
                generic.push(validated);
            }
            continue;
        };
        for (manager_name, package_list) in mapping {
            // The inner list is iterated before the key is looked up, so a
            // malformed entry under an unknown manager still raises.
            let definitions =
                package_list.as_array().map_or_else(Vec::new, Clone::clone);
            let mut validated = Vec::new();
            for definition in &definitions {
                validated.push(validate_entry(definition)?);
            }
            let Some(manager) = Manager::from_name(manager_name) else {
                log.error(
                    "distros/__init__.py",
                    &format!(
                        "Cannot install packages under '{manager_name}' as it is \
                         not a supported package manager!"
                    ),
                );
                continue;
            };
            if !by_manager.iter().any(|(m, _)| *m == manager) {
                by_manager.push((manager, Vec::new()));
            }
            let Some((_, slot)) = by_manager.iter_mut().find(|(m, _)| *m == manager)
            else {
                continue;
            };
            for entry in validated {
                let expanded = entry.expand();
                if !slot.iter().any(|held| held.expand() == expanded) {
                    slot.push(entry);
                }
            }
        }
    }
    Ok(Extracted {
        by_manager,
        generic,
    })
}

/// The `apt_get_*` keys of `system_info.distro`, read by `Apt.from_config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AptConfig {
    /// `apt_get_wrapper_command`, already resolved by [`apt_wrapper`].
    pub wrapper: Vec<String>,
    /// `apt_get_command`.
    pub command: Vec<String>,
    /// `apt_get_upgrade_subcommand`.
    pub upgrade_subcommand: String,
}

impl Default for AptConfig {
    fn default() -> Self {
        Self {
            wrapper: Vec::new(),
            command: APT_GET_COMMAND.iter().map(|s| (*s).to_string()).collect(),
            upgrade_subcommand: "dist-upgrade".to_owned(),
        }
    }
}

impl AptConfig {
    /// `Apt.apt_command`: the wrapper in front of the apt-get argv.
    fn apt_command(&self) -> Vec<String> {
        let mut argv = self.wrapper.clone();
        argv.extend(self.command.iter().cloned());
        argv
    }

    /// `Apt.from_config`, given `system_info.distro` and whether the wrapper
    /// command resolves on `PATH`.
    ///
    /// # Errors
    ///
    /// The `TypeError` upstream raises for an `apt_get_wrapper.command` that is
    /// neither a string nor a list.
    pub fn from_config(
        cfg: &Object,
        which: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Self, String> {
        let mut config = Self {
            wrapper: apt_wrapper(cfg.get("apt_get_wrapper"), which)?,
            ..Self::default()
        };
        // `if apt_get_command is None` only defaults the attribute; a present
        // key replaces the whole argv.
        if let Some(command) = cfg.get("apt_get_command").and_then(Value::as_array) {
            config.command = command.iter().map(py_str).collect();
        }
        if let Some(subcommand) = cfg.get("apt_get_upgrade_subcommand") {
            config.upgrade_subcommand = py_str(subcommand);
        }
        Ok(config)
    }
}

/// `apt.get_apt_wrapper`.
///
/// # Errors
///
/// The `TypeError` for a `command` that is neither a string nor a list.
pub fn apt_wrapper(
    cfg: Option<&Value>,
    which: &mut dyn FnMut(&str) -> bool,
) -> Result<Vec<String>, String> {
    // No section at all means `enabled="auto"` over the default command.
    let Some(cfg) = cfg.filter(|value| truthy(value)) else {
        return Ok(auto_wrapper(
            &DEFAULT_APT_WRAPPER
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
            which,
        ));
    };
    let enabled = cfg.get("enabled");
    let command = cfg.get("command");
    let command: Vec<String> = match command {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(list)) => list.iter().map(py_str).collect(),
        // A *missing* `command` lands here too, because `cfg.get` gives it
        // the same `None` a `command: null` would: once the section exists at
        // all, upstream demands a command in it. Writing the obvious
        // `apt_get_wrapper: {enabled: false}` therefore raises — see B73.
        None | Some(_) => {
            return Err("apt_wrapper command must be a string or list".to_owned())
        }
    };

    if enabled.is_some_and(is_true) {
        return Ok(command);
    }
    let auto = enabled.is_some_and(|value| py_str(value).to_lowercase() == "auto");
    if auto {
        return Ok(auto_wrapper(&command, which));
    }
    Ok(Vec::new())
}

/// The `"auto"` arm: use the command only if its first word is on `PATH`.
fn auto_wrapper(
    command: &[String],
    which: &mut dyn FnMut(&str) -> bool,
) -> Vec<String> {
    match command.first() {
        Some(first) if which(first) => command.to_vec(),
        _ => Vec::new(),
    }
}

/// `util.is_true`.
fn is_true(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => {
            matches!(s.to_lowercase().as_str(), "true" | "1" | "on" | "yes")
        }
        other => truthy(other),
    }
}

/// `Apt.run_package_command(command, args, pkgs)`.
#[must_use]
pub fn apt_package_command(
    config: &AptConfig,
    command: &str,
    args: &[String],
    pkgs: &[Entry],
) -> Step {
    let mut full_command = config.apt_command();
    full_command.extend(args.iter().cloned());
    // `upgrade` is the one subcommand whose spelling is configurable.
    full_command.push(if command == "upgrade" {
        config.upgrade_subcommand.clone()
    } else {
        command.to_owned()
    });
    full_command.extend(pkgs.iter().map(Entry::expand));
    Step {
        argv: full_command,
        env: vec![("DEBIAN_FRONTEND".to_owned(), "noninteractive".to_owned())],
        capture: false,
        wait_for_apt_lock: true,
        semaphore: None,
    }
}

/// `Apt.update_package_sources`.
#[must_use]
pub fn apt_update_sources(config: &AptConfig, force: bool) -> Step {
    Step {
        semaphore: Some((
            "update-sources".to_owned(),
            if force {
                Frequency::Always
            } else {
                Frequency::Instance
            },
        )),
        ..apt_package_command(config, "update", &[], &[])
    }
}

/// What the live system answers when a plan is being built.
///
/// Upstream asks these questions in the middle of installing; a plan has to
/// have them up front, exactly as [`crate::hostname`]'s does.
#[derive(Debug, Default, Clone)]
pub struct State {
    /// `Apt.available()` — `which("apt-get")`.
    pub apt_available: bool,
    /// `Snap.available()` — `which("snap")`.
    pub snap_available: bool,
    /// `Apt.get_all_packages()` — the `apt-cache pkgnames` set. `None` when it
    /// has not been asked, which makes every package look available.
    pub all_packages: Option<HashSet<String>>,
    /// `refresh.hold` out of `snap get system -d`, which is the one thing
    /// that stops `Snap.upgrade_packages` running `snap refresh`.
    ///
    /// The probe itself is still a step in the plan, because upstream runs it
    /// unconditionally and a recorded run has to show it.
    pub snap_refresh_hold: Option<String>,
}

impl State {
    fn available(&self, manager: Manager) -> bool {
        match manager {
            Manager::Apt => self.apt_available,
            Manager::Snap => self.snap_available,
        }
    }
}

/// `Apt.get_unavailable_packages`.
///
/// The suffixes are apt's own: `-` suppresses a transitive dependency, `^`
/// names a task, `/` pins a target release and `=` a version. All of them are
/// stripped before the name is looked up.
#[must_use]
pub fn unavailable<S: std::hash::BuildHasher>(
    all: &HashSet<String, S>,
    pkglist: &[String],
) -> Vec<String> {
    pkglist
        .iter()
        .filter(|pkg| {
            let base = pkg.split(['/', '=']).next().unwrap_or(pkg);
            !all.contains(base.trim_end_matches(['-', '^']))
        })
        .cloned()
        .collect()
}

/// `Apt.install_packages`.
fn apt_install(
    config: &AptConfig,
    state: &State,
    pkgs: &[Entry],
    log: &mut Logger,
) -> Vec<Step> {
    // Every install re-runs `update`, under its own once-per-instance
    // semaphore, so a second module installing packages does not re-fetch.
    let mut steps = vec![apt_update_sources(config, false)];
    let expanded: Vec<String> = pkgs.iter().map(Entry::expand).collect();
    let missing = state.all_packages.as_ref().map_or_else(Vec::new, |all| {
        // The availability check is made on the name alone, but what is
        // dropped from the command is the whole `name=version` spelling.
        let names: Vec<String> = expanded
            .iter()
            .map(|pkg| pkg.split('=').next().unwrap_or(pkg).to_owned())
            .collect();
        unavailable(all, &names)
    });
    if !missing.is_empty() {
        log.debug(
            "distros/package_management/apt.py",
            &format!(
                "The following packages were not found by APT so APT will not \
                 attempt to install them: {}",
                repr_list(&missing)
            ),
        );
    }
    let to_install: Vec<Entry> = pkgs
        .iter()
        .zip(&expanded)
        .filter(|(_, name)| {
            let base = name.split('=').next().unwrap_or(name);
            !missing.iter().any(|held| held == base)
        })
        .map(|(entry, _)| entry.clone())
        .collect();
    if !to_install.is_empty() {
        steps.push(apt_package_command(config, "install", &[], &to_install));
    }
    steps
}

/// `Snap.install_packages`: one `snap install` per package, because snap
/// reports neither availability nor per-package failure.
fn snap_install(pkgs: &[Entry]) -> Vec<Step> {
    pkgs.iter()
        .map(|entry| {
            let expanded = entry.expand();
            // `pkg.split("=", 1)`: a versioned entry becomes two argv words.
            let mut argv = vec!["snap".to_owned(), "install".to_owned()];
            match expanded.split_once('=') {
                Some((name, version)) => {
                    argv.push(name.to_owned());
                    argv.push(version.to_owned());
                }
                None => argv.push(expanded),
            }
            Step::plain(argv)
        })
        .collect()
}

/// What `Distro.install_packages` decided.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// The commands to run, in order.
    pub steps: Vec<Step>,
    /// `PackageInstallerError`, which upstream raises *after* the commands
    /// above have run — the packages no manager could take.
    pub failed: Option<String>,
}

/// `Distro.install_packages`, for a distro whose `package_managers` list is
/// the one given.
///
/// # Errors
///
/// The `ValueError` from `_validate_entry`, which happens before any command
/// is built. The `PackageInstallerError` is not an error here: it is raised
/// once the commands have run, so it rides in [`InstallPlan::failed`].
pub fn plan_install(
    managers: &[Manager],
    config: &AptConfig,
    state: &State,
    pkglist: &[Value],
    log: &mut Logger,
) -> Result<InstallPlan, String> {
    let extracted = extract(pkglist, log)?;
    let mut steps = Vec::new();
    let mut total_failed: Vec<String> = Vec::new();
    let mut generic = extracted.generic.clone();

    for &manager in managers {
        let mine: &[Entry] = extracted
            .by_manager
            .iter()
            .find(|(m, _)| *m == manager)
            .map_or(&[], |(_, packages)| packages.as_slice());
        let mut to_try = mine.to_vec();
        for entry in &generic {
            let expanded = entry.expand();
            if !to_try.iter().any(|held| held.expand() == expanded) {
                to_try.push(entry.clone());
            }
        }
        // Anything this manager will attempt is no longer someone else's
        // failure.
        let trying: HashSet<String> = to_try.iter().map(Entry::expand).collect();
        total_failed.retain(|pkg| !trying.contains(pkg));

        if !state.available(manager) {
            log.debug(
                "distros/__init__.py",
                &format!("Package manager '{}' not available", manager.name()),
            );
            total_failed.extend(trying);
            continue;
        }
        if to_try.is_empty() {
            continue;
        }
        // Only apt reports what it could not find; snap installs blind.
        let (mut produced, failed) = match manager {
            Manager::Apt => {
                let steps = apt_install(config, state, &to_try, log);
                let missing =
                    state.all_packages.as_ref().map_or_else(Vec::new, |all| {
                        let names: Vec<String> = to_try
                            .iter()
                            .map(|entry| {
                                let expanded = entry.expand();
                                expanded
                                    .split('=')
                                    .next()
                                    .unwrap_or(&expanded)
                                    .to_owned()
                            })
                            .collect();
                        unavailable(all, &names)
                    });
                (steps, missing)
            }
            Manager::Snap => (snap_install(&to_try), Vec::new()),
        };
        steps.append(&mut produced);
        if !failed.is_empty() {
            log.info("distros/__init__.py", &install_error(&failed));
        }
        total_failed.extend(failed.iter().cloned());
        // Whatever this manager could not take becomes the next one's
        // generic list — upstream narrows it rather than carrying it forward.
        let mine_expanded: HashSet<String> = mine.iter().map(Entry::expand).collect();
        generic = to_try
            .into_iter()
            .filter(|entry| {
                let expanded = entry.expand();
                failed.contains(&expanded) && !mine_expanded.contains(&expanded)
            })
            .collect();
    }

    // A manager the distro does not register but the config named explicitly
    // is still driven, from a default configuration.
    for (manager, packages) in &extracted.by_manager {
        if managers.contains(manager) {
            continue;
        }
        match manager {
            Manager::Apt => {
                steps.extend(apt_install(&AptConfig::default(), state, packages, log));
            }
            Manager::Snap => steps.extend(snap_install(packages)),
        }
    }

    if total_failed.is_empty() {
        Ok(InstallPlan {
            steps,
            failed: None,
        })
    } else {
        total_failed.sort();
        total_failed.dedup();
        Ok(InstallPlan {
            steps,
            failed: Some(install_error(&total_failed)),
        })
    }
}

/// The `error_message` both the log line and `PackageInstallerError` carry.
fn install_error(failed: &[String]) -> String {
    format!(
        "Failed to install the following packages: {}. See associated package \
         manager logs for more details.",
        repr_set(failed)
    )
}

/// `Distro.update_package_sources`.
#[must_use]
pub fn plan_update_sources(
    managers: &[Manager],
    config: &AptConfig,
    state: &State,
    force: bool,
    log: &mut Logger,
) -> Vec<Step> {
    let mut steps = Vec::new();
    for &manager in managers {
        if !state.available(manager) {
            log.debug(
                "distros/__init__.py",
                &format!(
                    "Skipping update for package manager '{}': not available.",
                    manager.name()
                ),
            );
            continue;
        }
        // `Snap.update_package_sources` is a documented no-op.
        if manager == Manager::Apt {
            steps.push(apt_update_sources(config, force));
        }
    }
    steps
}

/// `Distro.package_command`, which on the debian family accepts `upgrade` and
/// nothing else — plus Ubuntu's addition, which is to refresh snaps too.
///
/// `UbuntuDistro.package_command` calls `super()` first and then, if snap is
/// installed, `Snap.upgrade_packages()`: `snap get system -d` to read
/// `refresh.hold`, then `snap refresh` unless that hold is `forever`. It does
/// this for *any* command, but the debian half rejects everything except
/// `upgrade` before it gets there, so `upgrade` is the only one that arrives.
///
/// # Errors
///
/// The `RuntimeError` for any other subcommand.
pub fn plan_package_command(
    managers: &[Manager],
    config: &AptConfig,
    state: &State,
    command: &str,
) -> Result<Vec<Step>, String> {
    if command != "upgrade" {
        return Err(format!("Unable to handle {command} command"));
    }
    let mut steps = vec![apt_package_command(config, "upgrade", &[], &[])];
    if managers.contains(&Manager::Snap) && state.snap_available {
        let words = |argv: &[&str]| argv.iter().map(|w| (*w).to_owned()).collect();
        steps.push(Step::plain(words(&["snap", "get", "system", "-d"])));
        if state.snap_refresh_hold.as_deref() != Some("forever") {
            steps.push(Step::plain(words(&["snap", "refresh"])));
        }
    }
    Ok(steps)
}

/// Runs a plan, taking the apt locks and the semaphores each step asks for.
///
/// # Errors
///
/// The first step that fails, in upstream's `ProcessExecutionError` spelling.
pub fn run(
    steps: &[Step],
    runner: &mut dyn FnMut(&Step) -> Result<(), String>,
) -> Result<(), String> {
    for step in steps {
        runner(step)?;
    }
    Ok(())
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

    fn json(text: &str) -> Value {
        ci_config::yaml::load_yaml(text, ci_config::yaml::Limits::default()).unwrap()
    }

    fn list(text: &str) -> Vec<Value> {
        json(text).as_array().unwrap().clone()
    }

    fn argvs(steps: &[Step]) -> Vec<Vec<String>> {
        steps.iter().map(|step| step.argv.clone()).collect()
    }

    fn both_available() -> State {
        State {
            apt_available: true,
            snap_available: true,
            all_packages: None,
            snap_refresh_hold: None,
        }
    }

    #[test]
    fn a_pair_becomes_name_equals_version_unless_the_version_is_falsy() {
        assert_eq!(Entry::Plain("git".to_owned()).expand(), "git");
        assert_eq!(
            Entry::Pair(json("\"nginx\""), json("\"1.2\"")).expand(),
            "nginx=1.2"
        );
        // `if len(pkg) == 2 and pkg[1]` — the bare name, not `nginx=None`.
        assert_eq!(
            Entry::Pair(json("\"nginx\""), Value::Null).expand(),
            "nginx"
        );
        assert_eq!(
            Entry::Pair(json("\"nginx\""), json("\"\"")).expand(),
            "nginx"
        );
        // `%s` stringifies whatever it is handed.
        assert_eq!(
            Entry::Pair(json("\"nginx\""), json("12")).expand(),
            "nginx=12"
        );
    }

    #[test]
    fn an_entry_that_is_neither_a_string_nor_a_pair_is_a_value_error() {
        let expected =
            "Invalid 'packages' yaml specification. Check schema definition.";
        assert_eq!(validate_entry(&json("[a]")).unwrap_err(), expected);
        assert_eq!(validate_entry(&json("[a, b, c]")).unwrap_err(), expected);
        assert_eq!(validate_entry(&json("12")).unwrap_err(), expected);
        assert_eq!(validate_entry(&json("null")).unwrap_err(), expected);
    }

    #[test]
    fn manager_keys_split_the_list_and_bare_names_stay_generic() {
        let mut log = ci_log::Logger::silent();
        let extracted =
            extract(&list("[git, {apt: [vim]}, {snap: [core]}, curl]"), &mut log)
                .unwrap();
        assert_eq!(
            extracted.generic,
            vec![
                Entry::Plain("git".to_owned()),
                Entry::Plain("curl".to_owned())
            ]
        );
        assert_eq!(
            extracted.by_manager,
            vec![
                (Manager::Apt, vec![Entry::Plain("vim".to_owned())]),
                (Manager::Snap, vec![Entry::Plain("core".to_owned())]),
            ]
        );
    }

    #[test]
    fn an_unknown_manager_key_is_logged_and_dropped_not_raised() {
        let mut log = ci_log::Logger::silent();
        let extracted = extract(&list("[{yum: [vim]}, git]"), &mut log).unwrap();
        assert!(extracted.by_manager.is_empty());
        assert_eq!(extracted.generic, vec![Entry::Plain("git".to_owned())]);
    }

    #[test]
    fn a_malformed_entry_under_an_unknown_manager_still_raises() {
        // Upstream validates the inner list before it looks the key up, so
        // the `KeyError` arm is never reached.
        let mut log = ci_log::Logger::silent();
        assert!(extract(&list("[{yum: [[a, b, c]]}]"), &mut log).is_err());
    }

    #[test]
    fn the_wrapper_is_used_only_when_which_finds_it() {
        let mut found = |_: &str| true;
        let mut missing = |_: &str| false;
        assert_eq!(
            apt_wrapper(None, &mut found).unwrap(),
            vec!["eatmydata".to_owned()]
        );
        assert!(apt_wrapper(None, &mut missing).unwrap().is_empty());
        // An explicit `true` skips the `which` probe entirely.
        let enabled = json("{\"enabled\": true, \"command\": [\"nice\"]}");
        assert_eq!(
            apt_wrapper(Some(&enabled), &mut missing).unwrap(),
            vec!["nice".to_owned()]
        );
        let disabled = json("{\"enabled\": false, \"command\": [\"nice\"]}");
        assert!(apt_wrapper(Some(&disabled), &mut missing)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_wrapper_command_that_is_not_a_string_or_list_is_a_type_error() {
        let mut found = |_: &str| true;
        let bad = json("{\"enabled\": true, \"command\": 12}");
        assert_eq!(
            apt_wrapper(Some(&bad), &mut found).unwrap_err(),
            "apt_wrapper command must be a string or list"
        );
    }

    #[test]
    fn an_install_updates_the_sources_first_and_keeps_config_order() {
        let mut log = ci_log::Logger::silent();
        let plan = plan_install(
            &[Manager::Apt],
            &AptConfig::default(),
            &both_available(),
            &list("[vim, git, curl]"),
            &mut log,
        )
        .unwrap();
        assert!(plan.failed.is_none());
        let argv = argvs(&plan.steps);
        assert_eq!(argv[0].last().unwrap(), "update");
        // Config order, not the set order upstream would produce (B72).
        assert_eq!(
            &argv[1][argv[1].len() - 4..],
            ["install", "vim", "git", "curl"]
        );
    }

    #[test]
    fn packages_apt_does_not_have_are_dropped_and_reported() {
        let mut log = ci_log::Logger::silent();
        let state = State {
            all_packages: Some(["git".to_owned()].into_iter().collect()),
            ..both_available()
        };
        let plan = plan_install(
            &[Manager::Apt],
            &AptConfig::default(),
            &state,
            &list("[git, nosuchpkg]"),
            &mut log,
        )
        .unwrap();
        let argv = argvs(&plan.steps);
        assert_eq!(&argv[1][argv[1].len() - 2..], ["install", "git"]);
        assert_eq!(
            plan.failed.unwrap(),
            "Failed to install the following packages: {'nosuchpkg'}. See \
             associated package manager logs for more details."
        );
    }

    #[test]
    fn snap_gets_one_command_per_package_and_splits_the_version_off() {
        let mut log = ci_log::Logger::silent();
        let plan = plan_install(
            &[Manager::Snap],
            &AptConfig::default(),
            &State {
                apt_available: false,
                ..both_available()
            },
            &list("[{snap: [core, [hello, \"2.10\"]]}]"),
            &mut log,
        )
        .unwrap();
        assert_eq!(
            argvs(&plan.steps),
            vec![
                vec!["snap", "install", "core"],
                vec!["snap", "install", "hello", "2.10"],
            ]
        );
    }

    #[test]
    fn a_manager_that_is_not_installed_fails_its_packages_rather_than_the_boot() {
        let mut log = ci_log::Logger::silent();
        let plan = plan_install(
            &[Manager::Apt],
            &AptConfig::default(),
            &State {
                apt_available: false,
                ..both_available()
            },
            &list("[git]"),
            &mut log,
        )
        .unwrap();
        assert!(plan.steps.is_empty());
        assert!(plan.failed.unwrap().contains("{'git'}"));
    }

    #[test]
    fn update_sources_carries_a_once_per_instance_semaphore_unless_forced() {
        let mut log = ci_log::Logger::silent();
        let steps = plan_update_sources(
            &[Manager::Apt, Manager::Snap],
            &AptConfig::default(),
            &both_available(),
            false,
            &mut log,
        );
        // Snap's is a documented no-op, so only apt contributes.
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].semaphore,
            Some(("update-sources".to_owned(), Frequency::Instance))
        );
        let forced = plan_update_sources(
            &[Manager::Apt],
            &AptConfig::default(),
            &both_available(),
            true,
            &mut log,
        );
        assert_eq!(
            forced[0].semaphore,
            Some(("update-sources".to_owned(), Frequency::Always))
        );
    }

    #[test]
    fn package_command_takes_upgrade_and_refuses_everything_else() {
        let config = AptConfig::default();
        let apt = [Manager::Apt];
        let state = both_available();
        let steps = plan_package_command(&apt, &config, &state, "upgrade").unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].argv.last().unwrap(), "dist-upgrade");
        assert_eq!(
            steps[0].env,
            vec![("DEBIAN_FRONTEND".to_owned(), "noninteractive".to_owned())]
        );
        assert_eq!(
            plan_package_command(&apt, &config, &state, "install").unwrap_err(),
            "Unable to handle install command"
        );
    }

    /// `UbuntuDistro.package_command` adds `Snap.upgrade_packages` after the
    /// apt half: read `refresh.hold`, then refresh unless it is `forever`.
    #[test]
    fn ubuntu_refreshes_snaps_after_the_apt_upgrade() {
        let config = AptConfig::default();
        let ubuntu = [Manager::Apt, Manager::Snap];
        let mut state = both_available();

        let steps = plan_package_command(&ubuntu, &config, &state, "upgrade").unwrap();
        assert_eq!(
            argvs(&steps)[1..],
            [
                vec![
                    "snap".to_owned(),
                    "get".to_owned(),
                    "system".to_owned(),
                    "-d".to_owned()
                ],
                vec!["snap".to_owned(), "refresh".to_owned()],
            ]
        );

        // A hold that is not `forever` is still a refresh.
        state.snap_refresh_hold = Some("2030-01-01T00:00:00Z".to_owned());
        assert_eq!(
            plan_package_command(&ubuntu, &config, &state, "upgrade")
                .unwrap()
                .len(),
            3
        );

        state.snap_refresh_hold = Some("forever".to_owned());
        let held = plan_package_command(&ubuntu, &config, &state, "upgrade").unwrap();
        assert_eq!(held.len(), 2);
        assert_eq!(held[1].argv.last().unwrap(), "-d");

        // The probe is skipped outright when snap is not installed.
        state.snap_available = false;
        assert_eq!(
            plan_package_command(&ubuntu, &config, &state, "upgrade")
                .unwrap()
                .len(),
            1
        );
    }

    /// Once an `apt_get_wrapper` section exists, a `command` in it is not
    /// optional: the obvious `enabled: false` raises a `TypeError` out of the
    /// distro constructor, taking all of cloud-init with it. See B73.
    #[test]
    fn a_wrapper_section_without_a_command_is_a_type_error() {
        let mut which = |_: &str| true;
        let section = |text: &str| {
            ci_config::yaml::load_yaml(text, ci_config::yaml::Limits::default())
                .unwrap()
        };
        for text in [
            "enabled: false",
            "enabled: true",
            "command: null",
            "enabled: auto",
        ] {
            assert_eq!(
                apt_wrapper(Some(&section(text)), &mut which).unwrap_err(),
                "apt_wrapper command must be a string or list",
                "for {text:?}"
            );
        }
        // An empty section is falsy, so it takes the default path instead.
        assert_eq!(
            apt_wrapper(Some(&section("{}")), &mut which).unwrap(),
            vec!["eatmydata".to_owned()]
        );
    }

    #[test]
    fn apt_suffixes_are_stripped_before_the_availability_lookup() {
        let all: HashSet<String> = ["git".to_owned(), "build-essential".to_owned()]
            .into_iter()
            .collect();
        let asked = [
            "git/noble".to_owned(),
            "git=1:2.43".to_owned(),
            "build-essential^".to_owned(),
            "git-".to_owned(),
            "nope".to_owned(),
        ];
        assert_eq!(unavailable(&all, &asked), vec!["nope".to_owned()]);
    }
}
